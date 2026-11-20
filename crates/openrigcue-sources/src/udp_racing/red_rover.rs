//! Red Rover `S~KEY:VALUE …~E` text telemetry (listener `redrover`).

use openrigcue_model::Telemetry;

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::dict_channels::{DictValue, TelemetryDict};
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::parse_float;
use super::support::receiver::Receiver;
use crate::{Source, SourceError};

/// `NETWORK_PORT.redrover` as seeded by configuration migration 199.
/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 3001;
const RECEIVE_SIZE: usize = 1000;
const MAX_RPM: i64 = 7000;

/// The text between the first `first` and the next `last`, or
/// empty when either is missing.
fn find_between<'a>(s: &'a str, first: &str, last: &str) -> &'a str {
    let Some(start) = s.find(first).map(|i| i + first.len()) else { return "" };
    match s[start..].find(last) {
        Some(len) => &s[start..start + len],
        None => "",
    }
}

/// The square of `x`: `None` for an overflow (a finite `x` whose square overflows).
fn square(x: f64) -> Option<f64> {
    let s = x * x;
    (s.is_finite() || !x.is_finite()).then_some(s)
}

/// The speed the receive step derives from the velocity: `|v| · 0.036 / 3.6`, or 0 where that fails (a
/// missing or text component, an overflowing square).
fn speed(values: &TelemetryDict) -> f64 {
    let component = |key: &str| values.number(key).and_then(square);
    match (component("velox"), component("veloy"), component("veloz")) {
        (Some(x), Some(y), Some(z)) => (x + y + z).sqrt() * 0.036 / 3.6,
        _ => 0.0,
    }
}

/// Decodes a datagram; `None` where decoding fails (telemetry unchanged). Each
/// `KEY:VALUE` item is split at `:` and its first two parts used, so parts after the second are ignored.
fn decode(datagram: &[u8]) -> Option<TelemetryDict> {
    let text = std::str::from_utf8(datagram).ok()?;
    let mut values = TelemetryDict::default();
    for item in find_between(text, "S~", "~E").split(' ') {
        let mut parts = item.split(':');
        let (key, value) = (parts.next()?, parts.next()?);
        let v = parse_float(value).map_or_else(|| DictValue::Text(value.to_string()), DictValue::Number);
        values.set(&key.to_lowercase(), v);
    }
    let gear = values.get_mut("gear")?;
    let named_gear = match &*gear {
        DictValue::Text(t) if t == "N" => Some(0),
        DictValue::Text(t) if t == "R" => Some(-1),
        _ => None,
    };
    if let Some(number) = named_gear {
        *gear = DictValue::Integer(number);
    }
    let speed = speed(&values);
    values.set("speed", DictValue::Number(speed));
    values.set("max_rpm", DictValue::Integer(MAX_RPM));
    let jets = values.get("jets")?.clone();
    values.set("afterburner", jets);
    Some(values)
}

/// The Red Rover listener.
pub struct RedRoverSource {
    receiver: Receiver,
    datagram: Vec<u8>,
    /// The stored telemetry values, replaced by every good datagram.
    values: TelemetryDict,
}

impl RedRoverSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::with_imported_port(
                net,
                Endpoint::loopback(0, timeout_secs(1.0 / 7.0)),
                RECEIVE_SIZE,
                port,
            ),
            datagram: Vec::new(),
            values: TelemetryDict::default(),
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, timeout_secs(1.0 / 7.0));
        RedRoverSource {
            receiver: Receiver::new(net, endpoint, RECEIVE_SIZE),
            datagram: Vec::new(),
            values: TelemetryDict::default(),
        }
    }
}

impl Source for RedRoverSource {
    fn native_channel(&self, name: &str) -> Option<openrigcue_model::native_value::LegacyValue> {
        Some(self.values.native_channel(name))
    }

    fn id(&self) -> &'static str {
        "redrover"
    }

    /// The liveness test: true for a non-empty datagram.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_once() {
            Ok(data) => {
                self.datagram = data;
                !self.datagram.is_empty()
            }
            Err(_) => false,
        };
        Ok(running)
    }

    /// The receive step; raw stored values are read by the dispatcher later.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let decoded = decode(&self.datagram);
        let fresh = decoded.is_some();
        if let Some(values) = decoded {
            self.values = values;
        }
        Ok(fresh)
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.values.publish(telemetry)?;
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;

    #[test]
    fn native_cached_port_retry_retains_decoded_telemetry() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("imported port")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint.bind.ip(), std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(endpoint.timeout, Some(timeout_secs(1.0 / 7.0)));
                let step = {
                    let mut calls = self.0.lock().unwrap();
                    calls.push(port);
                    calls.len()
                };
                let port = port.for_bind()?;
                match step {
                    1 => Err(super::super::socket::socket_creation_error(io::Error::other("creation denied"))),
                    2 => Err(io::Error::other("bind denied")),
                    3 => Err(io::Error::other("timeout denied")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port);
                        self.1.bind(&endpoint)
                    }
                }
            }
        }
        let old_data = b"S~Gear:N Jets:0 Pitch:2~E";
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, b"S~Gear:R Jets:1 Pitch:7~E");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            RedRoverSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), good)));
        source.datagram = old_data.to_vec();
        source.values = decode(old_data).unwrap();
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.native_channel("pitch"), Some(openrigcue_model::native_value::LegacyValue::Float(2.0)));
        }
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("pitch"), Some(openrigcue_model::native_value::LegacyValue::Float(7.0)));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = RedRoverSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    #[test]
    fn find_between_like_the_original() {
        // The text between two markers
        assert_eq!(find_between("xxS~a:1 b:2~Eyy", "S~", "~E"), "a:1 b:2");
        assert_eq!(find_between("S~a:1", "S~", "~E"), "");
        assert_eq!(find_between("nothing", "S~", "~E"), "");
    }

    #[test]
    fn keys_gear_speed_and_jets() {
        // Receive step
        let net = FakeNetwork::default();
        let mut s = RedRoverSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, b"S~RPM:4000 Gear:R VeloX:300 VeloY:400 VeloZ:0 Jets:1 Pitch:2.5~E");
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.gear, t.max_rpm, t.afterburner, t.pitch), (4000.0, -1.0, 7000.0, 1.0, 2.5));
        assert_eq!(t.speed, 500.0 * 0.036 / 3.6);
        assert_eq!(s.native_channel("gear"), Some(openrigcue_model::native_value::LegacyValue::Int(-1)));
        assert_eq!(s.native_channel("max_rpm"), Some(openrigcue_model::native_value::LegacyValue::Int(7000)));
        assert_eq!(s.native_channel("rpm"), Some(openrigcue_model::native_value::LegacyValue::Float(4000.0)));
        assert_eq!(s.native_channel("missing"), Some(openrigcue_model::native_value::LegacyValue::Int(0)));
    }

    #[test]
    fn missing_gear_or_jets_drops_the_packet() {
        // Receive step: a missing gear or jets key fails -> false
        let net = FakeNetwork::default();
        let mut s = RedRoverSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        for bad in [&b"S~RPM:1 Jets:0~E"[..], b"S~Gear:N~E", b"S~Gear~E", b"S~~E"] {
            net.push(DEFAULT_PORT, bad);
            s.is_running().unwrap();
            let mut t = Telemetry::off();
            assert!(!s.poll(&mut t).unwrap(), "{bad:?}");
        }
    }

    fn poll_one(datagram: &[u8], mut t: Telemetry) -> (bool, Telemetry) {
        let net = FakeNetwork::default();
        let mut s = RedRoverSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, datagram);
        assert!(s.is_running().unwrap());
        let fresh = s.poll(&mut t).unwrap();
        (fresh, t)
    }

    #[test]
    fn items_with_extra_colons_use_the_first_two_parts() {
        // Receive step: an item is split at ':'; the second part is the value, the lower-cased first the key
        let (fresh, t) = poll_one(b"S~Gear:3 X:1:2 Jets:0 RPM:5:6~E", Telemetry::off());
        assert!(fresh);
        assert_eq!((t.rpm, t.gear), (5.0, 3.0));
    }

    #[test]
    fn text_values_take_the_dispatcher_fallback() {
        // Dispatcher: a non-numeric text has no integer value -> fuel_level default 0, raw_flag default -1;
        // pitch is stored as the text
        let before = Telemetry { fuel_level: 40.0, raw_flag: 2.0, pitch: 3.0, ..Telemetry::off() };
        let (fresh, t) = poll_one(b"S~Gear:2 Jets:0 Fuel_Level:abc Raw_Flag:x Pitch:abc~E", before);
        assert!(fresh);
        assert_eq!((t.fuel_level, t.raw_flag, t.pitch), (0.0, -1.0, 3.0));
    }

    #[test]
    fn an_overflowing_square_gives_zero_speed() {
        // Receive step: the square of 1e200 overflows -> speed = 0
        let (_, t) = poll_one(b"S~Gear:1 Jets:0 VeloX:1e200 VeloY:0 VeloZ:0~E", Telemetry::off());
        assert_eq!(t.speed, 0.0);
        // The square of 1e154 does not overflow; the sum does, without failing: speed inf
        assert_eq!(speed(&decode(b"S~Gear:1 Jets:0 VeloX:1e154 VeloY:1e154 VeloZ:1e154~E").unwrap()), f64::INFINITY);
    }

    #[test]
    fn an_infinite_velocity_makes_the_speed_unreadable() {
        // Receive step: the square of inf is inf (no overflow); the dispatcher's top-speed update
        // then finds no integer value: only car_name, track_loc and lateral_velocity are stored that tick
        let net = FakeNetwork::default();
        let mut s = RedRoverSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, b"S~Gear:1 Jets:0 RPM:4000 VeloX:inf VeloY:0 VeloZ:0~E");
        assert!(s.is_running().unwrap());
        let mut t = Telemetry { rpm: 1000.0, speed: 7.0, ..Telemetry::off() };
        crate::udp_racing::unhandled::assert_ends(s.poll(&mut t));
        assert_eq!((t.rpm, t.speed, t.gear), (1000.0, 7.0, 11.0));
    }

    #[test]
    fn velocity_text_gives_zero_speed_and_text_gear_keeps_the_value() {
        // Receive step: a text has no square -> speed 0; a non-numeric gear stays text
        let net = FakeNetwork::default();
        let mut s = RedRoverSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, b"S~Gear:P VeloX:fast Jets:0~E");
        s.is_running().unwrap();
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.speed, t.gear), (0.0, 11.0));
    }
}

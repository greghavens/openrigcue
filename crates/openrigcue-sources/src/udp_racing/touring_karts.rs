//! Touring Karts comma-separated telemetry (listener `touringkarts`).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::zero_listener_channels;
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::parse_float;
use super::support::receiver::Receiver;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 4123;
const RECEIVE_SIZE: usize = 320;

/// The stored telemetry values: each key exists once the receive step reached it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct KartValues {
    pitch: Option<f64>,
    yaw: Option<f64>,
    roll: Option<f64>,
    speed: Option<f64>,
}

/// The Touring Karts listener.
pub struct TouringKartsSource {
    receiver: Receiver,
    datagram: Vec<u8>,
    values: KartValues,
}

impl TouringKartsSource {
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
            values: KartValues::default(),
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, timeout_secs(1.0 / 7.0));
        TouringKartsSource {
            receiver: Receiver::new(net, endpoint, RECEIVE_SIZE),
            datagram: Vec::new(),
            values: KartValues::default(),
        }
    }

    /// The receive step: fields 1 (+1.0), 2, 0 and 8 of the comma list, assigned in that order until
    /// one fails to convert.
    fn decode(&mut self) -> bool {
        let Ok(text) = std::str::from_utf8(&self.datagram) else { return false };
        let fields: Vec<&str> = text.split(',').collect();
        let field = |i: usize| fields.get(i).and_then(|f| parse_float(f));
        let Some(pitch) = field(1) else { return false };
        self.values.pitch = Some(pitch + 1.0);
        let Some(yaw) = field(2) else { return false };
        self.values.yaw = Some(yaw);
        let Some(roll) = field(0) else { return false };
        self.values.roll = Some(roll);
        let Some(speed) = field(8) else { return false };
        self.values.speed = Some(speed);
        true
    }
}

impl Source for TouringKartsSource {
    fn id(&self) -> &'static str {
        "touringkarts"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = match name {
            "pitch" => self.values.pitch,
            "yaw" => self.values.yaw,
            "roll" => self.values.roll,
            "speed" => self.values.speed,
            _ => None,
        };
        Some(value.map_or(LegacyValue::Int(0), LegacyValue::Float))
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

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.decode())
    }

    /// Numeric compatibility view (the stored value, 0 when absent).
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        zero_listener_channels(telemetry);
        let v = self.values;
        telemetry.pitch = v.pitch.unwrap_or(0.0);
        telemetry.yaw = v.yaw.unwrap_or(0.0);
        telemetry.roll = v.roll.unwrap_or(0.0);
        telemetry.speed = v.speed.unwrap_or(0.0);
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;

    #[test]
    fn native_cached_port_setup_failures_preserve_partial_updates() {
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
        let old_values = KartValues { pitch: Some(1.0), yaw: Some(2.0), roll: Some(3.0), speed: Some(4.0) };
        let ready = FakeNetwork::default();
        ready.push_timeout(6001);
        ready.push(6001, b"8,9");
        ready.push(6001, b"1,2,3,0,0,0,0,0,10");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = TouringKartsSource::with_imported_network(
            ImportedPort::Valid(6001),
            Box::new(Staged(calls.clone(), ready)),
        );
        source.datagram = b"old".to_vec();
        source.values = old_values;
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, b"old");
            assert_eq!(source.values, old_values);
        }
        assert!(source.is_running().unwrap());
        assert!(!source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.values, KartValues { pitch: Some(10.0), ..old_values });
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.values, KartValues { pitch: Some(3.0), yaw: Some(3.0), roll: Some(1.0), speed: Some(10.0) });
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 6]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = TouringKartsSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    fn polled(datagrams: &[&[u8]]) -> (bool, Telemetry, TouringKartsSource) {
        let net = FakeNetwork::default();
        let mut s = TouringKartsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut t = Telemetry::off();
        let mut fresh = false;
        for d in datagrams {
            net.push(DEFAULT_PORT, d);
            assert!(s.is_running().unwrap());
            fresh = s.poll(&mut t).unwrap();
        }
        (fresh, t, s)
    }

    #[test]
    fn csv_columns() {
        // Receive step: roll = [0], pitch = [1] + 1.0, yaw = [2], speed = [8]
        let (fresh, t, source) = polled(&[b"1.5,-2.0,90,0,0,0,0,0,22.5"]);
        assert!(fresh);
        assert_eq!((t.roll, t.pitch, t.yaw, t.speed), (1.5, -1.0, 90.0, 22.5));
        assert_eq!(t.gear, 0.0);
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Float(22.5)));
        assert_eq!(source.native_channel("pitch"), Some(LegacyValue::Float(-1.0)));
        assert_eq!(source.native_channel("gear"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn partial_update_on_a_bad_field() {
        // Receive step: the values written before the failing conversion stay
        let (_, _, partial) = polled(&[b"0,-1,0"]);
        assert_eq!(partial.native_channel("pitch"), Some(LegacyValue::Float(0.0)));
        assert_eq!(partial.native_channel("speed"), Some(LegacyValue::Int(0)));
        let (fresh, t, source) = polled(&[b"1,2,3,0,0,0,0,0,4", b"9,8,7"]);
        assert!(!fresh);
        assert_eq!((t.pitch, t.yaw, t.roll, t.speed), (9.0, 7.0, 9.0, 4.0));
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Float(4.0)));
        assert_eq!(source.native_channel("pitch"), Some(LegacyValue::Float(9.0)));
    }
}

//! Vector 36 "XSim Telemetry" comma-separated stream (listener `vector36`).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::zero_listener_channels;
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::parse_float;
use super::support::receiver::Receiver;
use crate::{Source, SourceError};

/// `NETWORK_PORT.vector36` as seeded by configuration migration 291.
/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 4123;
const RECEIVE_SIZE: usize = 2000;
/// The column mapping: CSV column → channel and multiplier.
const MAPPING: [(&str, f64); 6] = [
    ("roll", 1.0),
    ("pitch", 1.0),
    ("yaw", 1.0),
    ("vertical_acceleration", 100.0),
    ("lateral_acceleration", 100.0),
    ("longitudinal_acceleration", 100.0),
];

/// The Vector 36 listener.
pub struct Vector36Source {
    receiver: Receiver,
    datagram: Vec<u8>,
    /// The stored values, by column: present once written.
    values: [Option<f64>; 6],
}

impl Vector36Source {
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
            values: [None; 6],
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, timeout_secs(1.0 / 7.0));
        Vector36Source { receiver: Receiver::new(net, endpoint, RECEIVE_SIZE), datagram: Vec::new(), values: [None; 6] }
    }

    /// The receive step: columns 0–5 in order until one fails to convert.
    fn decode(&mut self) -> bool {
        let Ok(text) = std::str::from_utf8(&self.datagram) else { return false };
        let columns: Vec<&str> = text.split(',').collect();
        for (i, (_, multiplier)) in MAPPING.iter().enumerate() {
            match columns.get(i).and_then(|c| parse_float(c)) {
                Some(v) => self.values[i] = Some(v * multiplier),
                None => return false,
            }
        }
        true
    }
}

impl Source for Vector36Source {
    fn id(&self) -> &'static str {
        "vector36"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = MAPPING.iter().position(|(channel, _)| *channel == name).and_then(|i| self.values[i]);
        Some(value.map_or(LegacyValue::Int(0), LegacyValue::Float))
    }

    /// The liveness test: a non-empty datagram.
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
        for ((channel, _), value) in MAPPING.iter().zip(self.values) {
            if let Some(slot) = telemetry.get_mut(channel) {
                *slot = value.unwrap_or(0.0);
            }
        }
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;

    #[test]
    fn native_cached_port_retry_preserves_fields_and_partial_decode() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("imported path")
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
                    1 => Err(super::super::socket::socket_creation_error(io::Error::other("create failed"))),
                    2 => Err(io::Error::other("bind failed")),
                    3 => Err(io::Error::other("timeout failed")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port);
                        self.1.bind(&endpoint)
                    }
                }
            }
        }
        let old_values = [Some(1.0), Some(2.0), Some(3.0), Some(4.0), Some(5.0), Some(6.0)];
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, b"8,9");
        good.push(6001, b"10,11,12,.1,.2,.3");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            Vector36Source::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), good)));
        source.values = old_values;
        source.datagram = b"old".to_vec();
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, b"old");
            assert_eq!(source.values, old_values);
        }
        assert!(source.is_running().unwrap());
        assert!(!source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.values, [Some(8.0), Some(9.0), Some(3.0), Some(4.0), Some(5.0), Some(6.0)]);
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.values, [Some(10.0), Some(11.0), Some(12.0), Some(10.0), Some(20.0), Some(30.0)]);
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 6]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = Vector36Source::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    #[test]
    fn columns_and_multipliers() {
        // Receive step: mapping 0..5, accelerations x100
        let net = FakeNetwork::default();
        let mut s = Vector36Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        assert_eq!(s.native_channel("roll"), Some(LegacyValue::Int(0)));
        net.push(DEFAULT_PORT, b"1.5,-2,3,0.01,0.02,-0.03,99");
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.roll, t.pitch, t.yaw), (1.5, -2.0, 3.0));
        assert_eq!((t.vertical_acceleration, t.lateral_acceleration, t.longitudinal_acceleration), (1.0, 2.0, -3.0));
        assert_eq!(t.speed, 0.0);
        assert_eq!(s.native_channel("roll"), Some(LegacyValue::Float(1.5)));
        assert_eq!(s.native_channel("longitudinal_acceleration"), Some(LegacyValue::Float(-3.0)));
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Int(0)));
        net.push(DEFAULT_PORT, b"4,5");
        assert!(s.is_running().unwrap());
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!((t.roll, t.pitch, t.yaw), (4.0, 5.0, 3.0), "columns before the failure are updated");
        assert_eq!(s.native_channel("pitch"), Some(LegacyValue::Float(5.0)));
        assert_eq!(s.native_channel("yaw"), Some(LegacyValue::Float(3.0)));
    }
}

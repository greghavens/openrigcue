//! The Crew and The Crew 2 "phys_out_sim" stream (listener `thecrew`): one 68-byte little-endian packet (an unsigned int, twelve floats, four unsigned ints).

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::zero_listener_channels;
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::{rad_to_deg, round_to};
use super::support::receiver::Receiver;
use super::support::wire::{Fields, Short};
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 1337;
const RECEIVE_SIZE: usize = 336;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
const PACKET_LEN: usize = 68;

/// The packet fields in use.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CrewPacket {
    yaw: f64,
    pitch: f64,
    roll: f64,
    acceleration: [f64; 3],
    velocity: [f64; 3],
}

impl CrewPacket {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        if f.len() != PACKET_LEN {
            return Err(Short);
        }
        Ok(CrewPacket {
            yaw: f.f32(16)?,
            pitch: f.f32(20)?,
            roll: f.f32(24)?,
            acceleration: [f.f32(28)?, f.f32(32)?, f.f32(36)?],
            velocity: [f.f32(40)?, f.f32(44)?, f.f32(48)?],
        })
    }

    /// The `speed` channel: `sqrt(vx·vx + vy·vy·vz·vz)`.
    fn speed(&self) -> f64 {
        let [vx, vy, vz] = self.velocity;
        (vx * vx + vy * vy * vz * vz).sqrt()
    }
}

/// The Crew listener.
pub struct TheCrewSource {
    receiver: Receiver,
    /// The last datagram: absent until the liveness test receives its first datagram.
    datagram: Option<Vec<u8>>,
    /// The last packet: absent until the receive step decodes its first datagram.
    packet: Option<CrewPacket>,
}

impl TheCrewSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::with_imported_port(net, Endpoint::loopback(0, RECEIVE_TIMEOUT), RECEIVE_SIZE, port),
            datagram: None,
            packet: None,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, RECEIVE_TIMEOUT);
        TheCrewSource { receiver: Receiver::new(net, endpoint, RECEIVE_SIZE), datagram: None, packet: None }
    }

    /// Writes the channels: any failure (no packet yet) answers 0.
    fn write_channels(&self, t: &mut Telemetry) {
        zero_listener_channels(t);
        let Some(p) = self.packet else { return };
        t.speed = p.speed();
        t.lateral_acceleration = round_to(p.acceleration[0], 6);
        t.vertical_acceleration = round_to(p.acceleration[2], 6);
        t.longitudinal_acceleration = round_to(p.acceleration[1], 6);
        t.yaw = rad_to_deg(p.yaw);
        t.roll = rad_to_deg(p.roll);
        t.pitch = rad_to_deg(p.pitch);
        t.lateral_velocity = round_to(p.velocity[0], 6);
        t.longitudinal_velocity = round_to(p.velocity[1], 6);
        t.vertical_velocity = round_to(p.velocity[2], 6);
    }
}

impl Source for TheCrewSource {
    fn id(&self) -> &'static str {
        "thecrew"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Bool, Float, Int};
        // Every mapped channel reads the packet; without one the read fails:
        // the failure is printed after "Issue" and the answer is false.
        let float = |value: fn(&CrewPacket) -> f64| {
            Some(match &self.packet {
                Some(packet) => Float(value(packet)),
                None => {
                    crate::printed::line("Issue name 'packet' is not defined");
                    Bool(false)
                }
            })
        };
        match name {
            "speed" => float(CrewPacket::speed),
            "lateral_acceleration" => float(|p| round_to(p.acceleration[0], 6)),
            "vertical_acceleration" => float(|p| round_to(p.acceleration[2], 6)),
            "longitudinal_acceleration" => float(|p| round_to(p.acceleration[1], 6)),
            "yaw" => float(|p| rad_to_deg(p.yaw)),
            "roll" => float(|p| rad_to_deg(p.roll)),
            "pitch" => float(|p| rad_to_deg(p.pitch)),
            "lateral_velocity" => float(|p| round_to(p.velocity[0], 6)),
            "longitudinal_velocity" => float(|p| round_to(p.velocity[1], 6)),
            "vertical_velocity" => float(|p| round_to(p.velocity[2], 6)),
            _ => Some(Int(0)),
        }
    }

    /// The liveness test: a 68-byte datagram.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_once() {
            Ok(data) => {
                let running = data.len() == PACKET_LEN;
                self.datagram = Some(data);
                running
            }
            Err(_) => false,
        };
        Ok(running)
    }

    /// The receive step: decode the datagram, which must be exactly
    /// 68 bytes; a failure is printed after "error".
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let parsed = match &self.datagram {
            None => Err("name 'data' is not defined".to_owned()),
            Some(data) => {
                CrewPacket::parse(data).map_err(|_| format!("unpack requires a buffer of {PACKET_LEN} bytes"))
            }
        };
        match parsed {
            Ok(packet) => {
                self.packet = Some(packet);
                Ok(true)
            }
            Err(text) => {
                crate::printed::line(format!("error {text}"));
                Ok(false)
            }
        }
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry);
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;

    fn datagram() -> Vec<u8> {
        let mut d = 1234u32.to_le_bytes().to_vec();
        let floats = [0.0f32, 0.0, 0.0, std::f32::consts::FRAC_PI_2, 0.1, -0.2, 1.5, 2.5, -9.81, 3.0, 2.0, 4.0];
        d.extend(floats.iter().flat_map(|v| v.to_le_bytes()));
        d.extend([0u8; 16]);
        d
    }

    #[test]
    fn native_port_setup_failures_retain_packet_until_successful_retry() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native cached port must reach bind_imported")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint.bind.ip(), std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(endpoint.timeout, Some(RECEIVE_TIMEOUT));
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
        let old_data = datagram();
        let old_packet = CrewPacket::parse(&old_data).unwrap();
        let mut new_data = old_data.clone();
        new_data[40..44].copy_from_slice(&6.0f32.to_le_bytes());
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, &new_data);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            TheCrewSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), good)));
        source.datagram = Some(old_data.clone());
        source.packet = Some(old_packet);
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram.as_deref(), Some(&old_data[..]));
            assert_eq!(source.packet, Some(old_packet));
        }
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("lateral_velocity"), Some(LegacyValue::Float(6.0)));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = TheCrewSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        source.packet = Some(old_packet);
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
        assert_eq!(source.packet, Some(old_packet));
    }

    #[test]
    fn channels() {
        // Channels: acceleration_x/z/y -> lateral/vertical/longitudinal, rad -> deg
        let net = FakeNetwork::default();
        let mut s = TheCrewSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        crate::printed::take();
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Bool(false)));
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Int(0)));
        // The receive step before any datagram: there is no datagram either.
        assert!(!s.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(crate::printed::take(), ["Issue name 'packet' is not defined", "error name 'data' is not defined"]);
        net.push(DEFAULT_PORT, &datagram());
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert!((t.yaw - 90.0).abs() < 1e-5);
        assert_eq!((t.lateral_acceleration, t.longitudinal_acceleration, t.vertical_acceleration), (1.5, 2.5, -9.81));
        assert_eq!((t.lateral_velocity, t.longitudinal_velocity, t.vertical_velocity), (3.0, 2.0, 4.0));
        // sqrt(3*3 + 2*2*4*4)
        assert_eq!(t.speed, (9.0f64 + 64.0).sqrt());
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Float((9.0f64 + 64.0).sqrt())));
        assert_eq!(s.native_channel("lateral_velocity"), Some(LegacyValue::Float(3.0)));
        assert_eq!(s.native_channel("vertical_acceleration"), Some(LegacyValue::Float(-9.81)));
        assert!(crate::printed::take().is_empty());
        s.datagram.as_mut().unwrap().truncate(PACKET_LEN - 1);
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(s.native_channel("lateral_velocity"), Some(LegacyValue::Float(3.0)));
        assert_eq!(crate::printed::take(), ["error unpack requires a buffer of 68 bytes"]);
    }

    #[test]
    fn only_68_byte_datagrams_count() {
        // Liveness: a 68-byte datagram
        let net = FakeNetwork::default();
        let mut s = TheCrewSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[0u8; 72]);
        assert!(!s.is_running().unwrap());
        assert_eq!(net.bound()[0].bind, "127.0.0.1:1337".parse().unwrap());
    }
}

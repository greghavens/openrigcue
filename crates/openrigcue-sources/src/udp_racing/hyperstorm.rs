//! HyperStorm (listener `hyperstorm`): seven little-endian floats per datagram.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::{native_int, trunc};
use super::support::receiver::Receiver;
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 20741;
const RECEIVE_SIZE: usize = 2000;
const PACKET_LEN: usize = 28;

/// One HyperStorm packet (seven little-endian floats).
#[derive(Debug, Clone, Copy, PartialEq)]
struct HyperStormPacket {
    pitch: f64,
    yaw: f64,
    roll: f64,
    lateral_acceleration: f64,
    vertical_acceleration: f64,
    longitudinal_acceleration: f64,
    speed: f64,
}

impl HyperStormPacket {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        Ok(HyperStormPacket {
            pitch: f.f32(0)?,
            yaw: f.f32(4)?,
            roll: f.f32(8)?,
            lateral_acceleration: f.f32(12)?,
            vertical_acceleration: f.f32(16)?,
            longitudinal_acceleration: f.f32(20)?,
            speed: f.f32(24)?,
        })
    }
}

/// The HyperStorm listener.
pub struct HyperStormSource {
    receiver: Receiver,
    datagram: Vec<u8>,
    packet: Option<HyperStormPacket>,
}

impl HyperStormSource {
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
            packet: None,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, timeout_secs(1.0 / 7.0));
        HyperStormSource { receiver: Receiver::new(net, endpoint, RECEIVE_SIZE), datagram: Vec::new(), packet: None }
    }

    /// Writes the channels from the stored packet, absent until the first packet, so
    /// every mapped read fails before it; a NaN or infinite speed has no integer value and fails too. Both end
    /// the tick after `lateral_velocity` ([`publish_speed_unreadable`]); `car_name` and `lateral_velocity` are
    /// not mapped and read 0.
    fn write_channels(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let Some((p, speed)) = self.packet.and_then(|p| Some((p, trunc(p.speed)?))) else {
            return publish_speed_unreadable(t, Some(String::new()), 0.0);
        };
        zero_listener_channels(t);
        t.speed = speed;
        t.lateral_acceleration = p.lateral_acceleration;
        t.vertical_acceleration = p.vertical_acceleration;
        t.longitudinal_acceleration = p.longitudinal_acceleration;
        t.yaw = p.yaw;
        t.roll = -p.roll;
        t.pitch = p.pitch;
        Ok(())
    }
}

impl Source for HyperStormSource {
    fn id(&self) -> &'static str {
        "hyperstorm"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let value = match name {
            "speed" => return native_int(self.packet?.speed),
            "lateral_acceleration" => self.packet?.lateral_acceleration,
            "vertical_acceleration" => self.packet?.vertical_acceleration,
            "longitudinal_acceleration" => self.packet?.longitudinal_acceleration,
            "yaw" => self.packet?.yaw,
            "roll" => -self.packet?.roll,
            "pitch" => self.packet?.pitch,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    /// The liveness test: a 28-byte datagram.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_once() {
            Ok(data) => {
                self.datagram = data;
                self.datagram.len() == PACKET_LEN
            }
            Err(_) => false,
        };
        Ok(running)
    }

    /// The receive step.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.datagram.len() == PACKET_LEN;
        if fresh {
            self.packet = HyperStormPacket::parse(&self.datagram).ok();
        }
        Ok(fresh)
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry)?;
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;
    use crate::udp_racing::unhandled::assert_ends;

    fn datagram(values: [f32; 7]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn native_receive_keeps_nonfinite_getters_and_failed_packet_retention_separate() {
        let mut source =
            HyperStormSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(FakeNetwork::default()));
        source.datagram = datagram([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, f32::NAN]);
        let before = Telemetry { speed: 23.0, roll: 19.0, ..Telemetry::off() };
        let mut compatibility = before.clone();
        assert!(source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before);
        assert_eq!(source.read_native("speed"), crate::NativeRead::Raised);
        assert_eq!(source.native_channel("roll"), Some(LegacyValue::Float(-3.0)));
        source.datagram.pop();
        assert!(!source.receive_native(&mut compatibility).unwrap());
        assert_eq!(source.native_channel("roll"), Some(LegacyValue::Float(-3.0)), "bad length retains packet");
        assert_eq!(compatibility, before);
    }

    #[test]
    fn native_cached_port_retry_preserves_packet_and_original_length_gate() {
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
        let old_data = datagram([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        let old_packet = HyperStormPacket::parse(&old_data).unwrap();
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, &[0; 27]);
        good.push(6001, &datagram([2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            HyperStormSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), good)));
        source.datagram = old_data.clone();
        source.packet = Some(old_packet);
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.packet, Some(old_packet));
        }
        assert!(!source.is_running().unwrap());
        assert!(!source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.packet, Some(old_packet));
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(8)));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 6]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = HyperStormSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    #[test]
    fn check_status_wants_28_bytes_on_loopback() {
        // Liveness: bind 127.0.0.1, timeout 1/7 s, a 28-byte datagram
        let net = FakeNetwork::default();
        let mut s = HyperStormSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[0u8; 27]);
        assert!(!s.is_running().unwrap());
        net.push(DEFAULT_PORT, &[0u8; 28]);
        assert!(s.is_running().unwrap());
        assert_eq!(net.bound()[0].bind, "127.0.0.1:20741".parse().unwrap());
        assert_eq!(net.bound()[0].timeout, Some(timeout_secs(1.0 / 7.0)));
    }

    #[test]
    fn channels() {
        // Channels: speed as an integer, roll negated, others as sent, else 0
        let net = FakeNetwork::default();
        let mut s = HyperStormSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &datagram([1.5, 2.5, 3.5, 0.1, 0.2, 0.3, 12.9]));
        s.is_running().unwrap();
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.pitch, t.yaw, t.roll, t.speed), (1.5, 2.5, -3.5, 12.0));
        assert_eq!(t.lateral_acceleration, f64::from(0.1f32));
        assert_eq!(t.longitudinal_acceleration, f64::from(0.3f32));
        assert_eq!(t.gear, 0.0);
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Int(12)));
        assert_eq!(s.native_channel("roll"), Some(LegacyValue::Float(-3.5)));
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // A NaN speed has no integer value; the dispatcher's top-speed update does
        // not handle that, so only car_name (0), track_loc (0) and lateral_velocity (0) are stored
        let net = FakeNetwork::default();
        let mut s = HyperStormSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &datagram([1.5, 2.5, 3.5, 0.1, 0.2, 0.3, f32::NAN]));
        s.is_running().unwrap();
        let before =
            Telemetry { speed: 9.0, pitch: 4.0, lateral_velocity: 1.0, vehicle: "v".into(), ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { vehicle: String::new(), lateral_velocity: 0.0, ..before });
        assert!(s.channel_read_raised("speed"));
        assert_eq!(s.native_channel("speed"), None);
        assert_eq!(s.native_channel("pitch"), Some(LegacyValue::Float(1.5)));
        assert!(!s.channel_read_raised("pitch"));
        use crate::derive::{Listener, SourceListener};
        let mut listener = SourceListener::new(s);
        listener.receive();
        assert_eq!(listener.channel("speed"), None);
        assert_eq!(listener.native_channel("speed"), None);
    }

    #[test]
    fn before_the_first_packet_mapped_reads_raise() {
        // No packet yet: the speed read fails
        let net = FakeNetwork::default();
        let mut s = HyperStormSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net));
        let before = Telemetry { speed: 9.0, pitch: 4.0, vehicle: "v".into(), ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { vehicle: String::new(), ..before });
        assert!(s.channel_read_raised("speed"));
        assert!(s.channel_read_raised("pitch"));
        assert_eq!(s.native_channel("pitch"), None);
        assert!(!s.channel_read_raised("rpm"));
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Int(0)));
    }
}

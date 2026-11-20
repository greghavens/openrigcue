//! MotoGP 18 (listener `motogp`): three byte ranges of the game's UDP packet are decoded.

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::{int_text, round_to, trunc};
use super::support::receiver::Receiver;
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 7100;
const RECEIVE_SIZE: usize = 320;

/// The byte-packed MotoGP 18 values, built from bytes 187..196, 216..224 and 284..296 of the datagram.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MotoGpPacket {
    max_rpm: f64,
    gear: i8,
    rpm: f64,
    suspension_position_f: f64,
    suspension_position_r: f64,
    lateral_g_force: f64,
    longitudinal_g_force: f64,
    speed: f64,
}

impl MotoGpPacket {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        // Decoding needs the 29 bytes of the three slices.
        f.bytes(284, 12)?;
        Ok(MotoGpPacket {
            max_rpm: f.f32(187)?,
            gear: f.i8(191)?,
            rpm: f.f32(192)?,
            suspension_position_f: f.f32(216)?,
            suspension_position_r: f.f32(220)?,
            lateral_g_force: f.f32(284)?,
            longitudinal_g_force: f.f32(288)?,
            speed: f.f32(292)?,
        })
    }
}

/// The MotoGP 18 listener.
pub struct MotoGpSource {
    receiver: Receiver,
    datagram: Vec<u8>,
    packet: Option<MotoGpPacket>,
}

impl MotoGpSource {
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
        MotoGpSource { receiver: Receiver::new(net, endpoint, RECEIVE_SIZE), datagram: Vec::new(), packet: None }
    }

    /// Writes the channels (the numeric strings answered are what the dispatcher converts). Where a
    /// NaN or infinite rpm or max rpm has no integer value the dispatcher keeps the previous value; where the
    /// speed has none the tick ends after `lateral_velocity` ([`publish_speed_unreadable`]; `car_name` and
    /// `lateral_velocity` are not mapped and read 0).
    fn write_channels(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let Some(p) = self.packet else {
            zero_listener_channels(t);
            return Ok(());
        };
        let Some(speed) = trunc(p.speed) else {
            return publish_speed_unreadable(t, Some(String::new()), 0.0);
        };
        let (rpm, max_rpm) = (t.rpm, t.max_rpm);
        zero_listener_channels(t);
        t.rpm = trunc(p.rpm).unwrap_or(rpm);
        t.max_rpm = trunc(p.max_rpm).unwrap_or(max_rpm);
        t.gear = f64::from(p.gear);
        t.speed = speed;
        t.suspension_travel_lf = p.suspension_position_f / 1000.0;
        t.suspension_travel_rf = p.suspension_position_f / 1000.0;
        t.suspension_travel_lr = p.suspension_position_r / 1000.0;
        t.suspension_travel_rr = p.suspension_position_r / 1000.0;
        t.steering_wheel_angle = round_to(p.lateral_g_force, 6);
        t.vertical_acceleration = 0.0;
        t.lateral_acceleration = p.lateral_g_force;
        t.longitudinal_acceleration = p.longitudinal_g_force;
        Ok(())
    }
}

impl Source for MotoGpSource {
    fn id(&self) -> &'static str {
        "motogp"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let Some(p) = self.packet else { return Some(LegacyValue::Bool(false)) };
        let value = match name {
            "rpm" => return int_text(p.rpm).map(LegacyValue::Str),
            "max_rpm" => return int_text(p.max_rpm).map(LegacyValue::Str),
            "gear" => return Some(LegacyValue::Str(p.gear.to_string())),
            "speed" => return int_text(p.speed).map(LegacyValue::Str),
            "suspension_travel_lf" | "suspension_travel_rf" => p.suspension_position_f / 1000.0,
            "suspension_travel_lr" | "suspension_travel_rr" => p.suspension_position_r / 1000.0,
            "steering_wheel_angle" => round_to(p.lateral_g_force, 6),
            "lateral_acceleration" => p.lateral_g_force,
            "longitudinal_acceleration" => p.longitudinal_g_force,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
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

    /// The receive step.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let parsed = MotoGpPacket::parse(&self.datagram).ok();
        if parsed.is_some() {
            self.packet = parsed;
        }
        Ok(parsed.is_some())
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

    /// A MotoGP 18 datagram with the decoded byte ranges filled in.
    fn datagram() -> Vec<u8> {
        let mut d = vec![0u8; 300];
        d[187..191].copy_from_slice(&14000.7f32.to_le_bytes());
        d[191] = 0xFF;
        d[192..196].copy_from_slice(&9000.2f32.to_le_bytes());
        d[216..220].copy_from_slice(&50.0f32.to_le_bytes());
        d[220..224].copy_from_slice(&80.0f32.to_le_bytes());
        d[284..288].copy_from_slice(&1.234_567_9_f32.to_le_bytes());
        d[288..292].copy_from_slice(&(-0.5f32).to_le_bytes());
        d[292..296].copy_from_slice(&40.6f32.to_le_bytes());
        d
    }

    #[test]
    fn native_cached_port_setup_failures_preserve_the_previous_packet() {
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
        let old_data = datagram();
        let old_packet = MotoGpPacket::parse(&old_data).unwrap();
        let mut next_data = old_data.clone();
        next_data[292..296].copy_from_slice(&25.0f32.to_le_bytes());
        let ready = FakeNetwork::default();
        ready.push_timeout(6001);
        ready.push(6001, &next_data);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            MotoGpSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), ready)));
        source.datagram = old_data.clone();
        source.packet = Some(old_packet);
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.packet, Some(old_packet));
        }
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("25".into())));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = MotoGpSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    #[test]
    fn channels_from_the_sliced_packet() {
        // Receive step (bytes 187..196, 216..224, 284..296) and channels
        let net = FakeNetwork::default();
        let mut s = MotoGpSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &datagram());
        assert!(s.is_running().unwrap());
        assert_eq!(net.bound()[0].bind, "127.0.0.1:7100".parse().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (9000.0, 14000.0, -1.0, 40.0));
        assert_eq!((t.suspension_travel_lf, t.suspension_travel_rr), (0.05, 0.08));
        assert_eq!(t.steering_wheel_angle, 1.234568);
        assert_eq!(t.lateral_acceleration, f64::from(1.234_567_9_f32));
        assert_eq!((t.longitudinal_acceleration, t.vertical_acceleration), (-0.5, 0.0));
        for (name, expected) in [
            ("rpm", LegacyValue::Str("9000".into())),
            ("max_rpm", LegacyValue::Str("14000".into())),
            ("gear", LegacyValue::Str("-1".into())),
            ("speed", LegacyValue::Str("40".into())),
            ("suspension_travel_lf", LegacyValue::Float(0.05)),
            ("steering_wheel_angle", LegacyValue::Float(1.234568)),
            ("vertical_acceleration", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(expected), "{name}");
        }
        s.datagram[187..191].copy_from_slice(&f32::INFINITY.to_le_bytes());
        s.datagram[192..196].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(s.poll(&mut t).unwrap());
        assert!(s.channel_read_raised("rpm"));
        assert!(s.channel_read_raised("max_rpm"));
        assert_eq!(s.native_channel("rpm"), None);
        assert_eq!(s.native_channel("max_rpm"), None);
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Str("40".into())));
    }

    #[test]
    fn short_packet_keeps_the_previous_one() {
        // Receive step: a short buffer cannot be decoded -> false
        let net = FakeNetwork::default();
        let mut s = MotoGpSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Bool(false)));
        assert_eq!(s.native_channel("unknown"), Some(LegacyValue::Bool(false)));
        assert!(!s.channel_read_raised("rpm"));
        net.push(DEFAULT_PORT, &datagram());
        net.push(DEFAULT_PORT, &[1u8; 200]);
        s.is_running().unwrap();
        let mut t = Telemetry::off();
        s.poll(&mut t).unwrap();
        s.is_running().unwrap();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 9000.0);
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Str("9000".into())));
        assert_eq!(s.native_channel("unknown"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // A NaN speed has no integer value; the dispatcher's top-speed update does
        // not handle that, so only car_name (0), track_loc (0) and lateral_velocity (0) are stored
        let net = FakeNetwork::default();
        let mut s = MotoGpSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut d = datagram();
        d[292..296].copy_from_slice(&f32::NAN.to_le_bytes());
        net.push(DEFAULT_PORT, &d);
        s.is_running().unwrap();
        let before = Telemetry { rpm: 1.0, speed: 9.0, lateral_velocity: 2.0, vehicle: "v".into(), ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { vehicle: String::new(), lateral_velocity: 0.0, ..before });
        assert!(s.channel_read_raised("speed"));
        assert_eq!(s.native_channel("speed"), None);
    }
}

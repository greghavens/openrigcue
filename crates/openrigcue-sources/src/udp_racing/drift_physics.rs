//! FURIDASHI: Drift Cyber Sport and RDS: The Official Drift Videogame, "Format B" motion data (listener
//! `driftphysicscrew`).

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{timeout_secs, Endpoint, Network, SystemNetwork};
use super::support::pymath::{native_int, rad_to_deg, trunc};
use super::support::receiver::{ReceiveFailure, Receiver};
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// `NETWORK_PORT.driftphysicscrew` as seeded by configuration migration 152.
/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 20782;
const RECEIVE_SIZE: usize = 256;
const PACKET_LEN: usize = 64;
const KMH_TO_MS: f64 = 0.277778;
const MAX_RPM: f64 = 9900.0;

/// The "Format B" motion packet: two `u32`, 13 floats and an `i32`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MotionDataFormatB {
    pitch: f64,
    roll: f64,
    headings: f64,
    acc_longitude: f64,
    acc_latitude: f64,
    acc_vertical: f64,
    suspension_fr: f64,
    suspension_fl: f64,
    suspension_rr: f64,
    suspension_rl: f64,
    /// km/h.
    speed: f64,
    /// Fraction of 10000 rpm.
    rpm: f64,
    gear: i32,
}

impl MotionDataFormatB {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        Ok(MotionDataFormatB {
            pitch: f.f32(8)?,
            roll: f.f32(12)?,
            headings: f.f32(16)?,
            acc_longitude: f.f32(20)?,
            acc_latitude: f.f32(24)?,
            acc_vertical: f.f32(28)?,
            suspension_fr: f.f32(36)?,
            suspension_fl: f.f32(40)?,
            suspension_rr: f.f32(44)?,
            suspension_rl: f.f32(48)?,
            speed: f.f32(52)?,
            rpm: f.f32(56)?,
            gear: f.i32(60)?,
        })
    }
}

/// The Drift Physics Crew listener.
pub struct DriftPhysicsSource {
    receiver: Receiver,
    native_setup: bool,
    datagram: Vec<u8>,
    sim: Option<MotionDataFormatB>,
}

impl DriftPhysicsSource {
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
            native_setup: true,
            datagram: Vec::new(),
            sim: None,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, timeout_secs(1.0 / 7.0));
        DriftPhysicsSource {
            receiver: Receiver::new(net, endpoint, RECEIVE_SIZE),
            native_setup: false,
            datagram: Vec::new(),
            sim: None,
        }
    }

    fn check_status_with_report(&mut self, mut report: impl FnMut(String)) -> bool {
        match self.receiver.receive_once() {
            Ok(data) => {
                self.datagram = data;
                !self.datagram.is_empty()
            }
            Err(ReceiveFailure::Bind(error) | ReceiveFailure::Receive(error)) => {
                // Socket, bind and receive failures are printed.
                report(crate::printed::os_error(&error));
                false
            }
        }
    }

    /// Writes the channels: 0 for everything until a packet arrived. Where a NaN or infinite
    /// rpm has no integer value the dispatcher keeps the previous rpm; where the speed has none the tick ends after
    /// `lateral_velocity` ([`publish_speed_unreadable`]; `car_name` and `lateral_velocity` are not mapped and
    /// read 0).
    fn write_channels(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let Some(s) = self.sim else {
            zero_listener_channels(t);
            return Ok(());
        };
        let Some(speed) = trunc(s.speed * KMH_TO_MS) else {
            return publish_speed_unreadable(t, Some(String::new()), 0.0);
        };
        let rpm = t.rpm;
        zero_listener_channels(t);
        t.rpm = trunc(s.rpm * 10000.0).unwrap_or(rpm);
        t.max_rpm = MAX_RPM;
        t.gear = f64::from(s.gear);
        t.speed = speed;
        t.lateral_acceleration = s.acc_latitude;
        t.vertical_acceleration = s.acc_vertical;
        t.longitudinal_acceleration = s.acc_longitude;
        t.roll = -rad_to_deg(s.roll);
        t.pitch = rad_to_deg(s.pitch);
        t.yaw = rad_to_deg(s.headings);
        t.suspension_travel_rr = s.suspension_rr;
        t.suspension_travel_rf = s.suspension_fr;
        t.suspension_travel_lf = s.suspension_fl;
        t.suspension_travel_lr = s.suspension_rl;
        Ok(())
    }
}

impl Source for DriftPhysicsSource {
    fn id(&self) -> &'static str {
        "driftphysicscrew"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let Some(s) = self.sim else { return Some(LegacyValue::Int(0)) };
        let value = match name {
            "rpm" => return native_int(s.rpm * 10000.0),
            "max_rpm" => return Some(LegacyValue::Str("9900".into())),
            "gear" => return Some(LegacyValue::Int(i64::from(s.gear))),
            "speed" => return native_int(s.speed * KMH_TO_MS),
            "lateral_acceleration" => s.acc_latitude,
            "vertical_acceleration" => s.acc_vertical,
            "longitudinal_acceleration" => s.acc_longitude,
            "roll" => -rad_to_deg(s.roll),
            "pitch" => rad_to_deg(s.pitch),
            "yaw" => rad_to_deg(s.headings),
            "suspension_travel_rr" => s.suspension_rr,
            "suspension_travel_rf" => s.suspension_fr,
            "suspension_travel_lf" => s.suspension_fl,
            "suspension_travel_lr" => s.suspension_rl,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    /// The liveness test: whether a datagram arrived.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let native_setup = self.native_setup;
        Ok(self.check_status_with_report(|message| {
            if native_setup {
                println!("{message}");
            }
        }))
    }

    /// The receive step: only 64-byte datagrams are decoded.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.datagram.len() == PACKET_LEN;
        if fresh {
            self.sim = MotionDataFormatB::parse(&self.datagram).ok();
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

    fn datagram() -> Vec<u8> {
        let mut d = Vec::new();
        d.extend(0xC0FFEEu32.to_le_bytes());
        d.extend(7u32.to_le_bytes());
        let floats = [0.1f32, 0.2, 0.3, 1.0, 2.0, 3.0, 0.0, 0.11, 0.12, 0.13, 0.14, 100.0, 0.55];
        d.extend(floats.iter().flat_map(|v| v.to_le_bytes()));
        d.extend(4i32.to_le_bytes());
        d
    }

    #[test]
    fn native_setup_and_receive_failures_print_the_original_error_text() {
        use super::super::support::net::Datagrams;
        use std::io;
        struct Failure(bool);
        impl Network for Failure {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                if self.0 {
                    Err(super::super::socket::socket_creation_error(io::Error::other("allocation denied")))
                } else {
                    Err(io::Error::other("bind denied"))
                }
            }
        }
        for (creation, expected) in [(true, "allocation denied"), (false, "bind denied")] {
            let mut source = DriftPhysicsSource::with_network(
                ListenerSettings::with_port(DEFAULT_PORT),
                Box::new(Failure(creation)),
            );
            let mut reports = Vec::new();
            assert!(!source.check_status_with_report(|message| reports.push(message)));
            assert_eq!(reports, [expected]);
        }
        let mut source = DriftPhysicsSource::with_network(
            ListenerSettings::with_port(DEFAULT_PORT),
            Box::new(FakeNetwork::default()),
        );
        let mut reports = Vec::new();
        assert!(!source.check_status_with_report(|message| reports.push(message)));
        assert_eq!(reports, ["timed out"]);
    }

    #[test]
    fn native_cached_port_retries_preserve_the_previous_motion() {
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
        let old_data = datagram();
        let old_motion = MotionDataFormatB::parse(&old_data).unwrap();
        let mut new_data = old_data.clone();
        new_data[52..56].copy_from_slice(&36.0f32.to_le_bytes());
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, &new_data);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            DriftPhysicsSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), good)));
        source.datagram = old_data.clone();
        source.sim = Some(old_motion);
        let mut reports = Vec::new();
        for _ in 0..4 {
            assert!(!source.check_status_with_report(|message| reports.push(message)));
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.sim, Some(old_motion));
        }
        assert_eq!(reports, ["create failed", "bind failed", "timeout failed", "timed out"]);
        assert!(source.check_status_with_report(|_| panic!("success")));
        source.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(10)));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = DriftPhysicsSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        let mut reports = Vec::new();
        assert!(!source.check_status_with_report(|message| reports.push(message)));
        assert_eq!(reports, ["bind(): port must be 0-65535."]);
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    #[test]
    fn channels() {
        // Channel values
        let net = FakeNetwork::default();
        let mut s = DriftPhysicsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &datagram());
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear), ((f64::from(0.55f32) * 10000.0).trunc(), 9900.0, 4.0));
        assert_eq!(t.speed, 27.0, "int(100 * 0.277778)");
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.vertical_acceleration), (1.0, 2.0, 3.0));
        assert_eq!(t.roll, -rad_to_deg(f64::from(0.2f32)));
        assert_eq!(t.yaw, rad_to_deg(f64::from(0.3f32)));
        assert_eq!(t.suspension_travel_rf, f64::from(0.11f32));
        assert_eq!(t.suspension_travel_lr, f64::from(0.14f32));
        for (name, expected) in [
            ("rpm", LegacyValue::Int(5500)),
            ("max_rpm", LegacyValue::Str("9900".into())),
            ("gear", LegacyValue::Int(4)),
            ("speed", LegacyValue::Int(27)),
            ("suspension_travel_rf", LegacyValue::Float(f64::from(0.11f32))),
            ("lateral_acceleration", LegacyValue::Float(2.0)),
            ("yaw_rate", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(expected), "{name}");
        }
        let mut d = datagram();
        d[56..60].copy_from_slice(&f32::NAN.to_le_bytes());
        net.push(DEFAULT_PORT, &d);
        assert!(s.is_running().unwrap());
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 5500.0, "the compatibility frame retains its previous numeric value");
        assert!(s.channel_read_raised("rpm"));
        assert_eq!(s.native_channel("rpm"), None);
        use crate::derive::{Listener, SourceListener};
        let mut listener = SourceListener::new(s);
        listener.receive();
        assert_eq!(listener.channel("rpm"), None, "the failed native getter must hide the older frame");
        assert_eq!(listener.native_channel("rpm"), None);
        assert_eq!(listener.native_channel("max_rpm"), Some(LegacyValue::Str("9900".into())));
    }

    #[test]
    fn other_lengths_are_ignored() {
        // A datagram that is not 64 bytes is not taken in
        let net = FakeNetwork::default();
        let mut s = DriftPhysicsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[0u8; 63]);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.max_rpm, 0.0, "sim is None -> 0");
        assert_eq!(s.native_channel("max_rpm"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Int(0)));
        assert!(!s.channel_read_raised("speed"));
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // An infinite speed has no integer value; the dispatcher's top-speed update does not
        // handle that, so only car_name (0), track_loc (0) and lateral_velocity (0) are stored
        let net = FakeNetwork::default();
        let mut s = DriftPhysicsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut d = datagram();
        d[52..56].copy_from_slice(&f32::INFINITY.to_le_bytes());
        net.push(DEFAULT_PORT, &d);
        assert!(s.is_running().unwrap());
        let before = Telemetry { rpm: 1.0, speed: 9.0, lateral_velocity: 2.0, vehicle: "v".into(), ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { vehicle: String::new(), lateral_velocity: 0.0, ..before });
        assert_eq!(s.native_channel("speed"), None);
        assert!(s.channel_read_raised("speed"));
        assert_eq!(s.native_channel("max_rpm"), Some(LegacyValue::Str("9900".into())));
        assert!(!s.channel_read_raised("rpm"));
    }
}

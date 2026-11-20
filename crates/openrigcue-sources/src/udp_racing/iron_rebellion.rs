//! Iron Rebellion (listener `ironrebellion`): an 11-float structure per datagram.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::{int_text, trunc};
use super::support::receiver::{ReceiveFailure, Receiver};
use super::support::wire::Fields;
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// `NETWORK_PORT.ironrebellion` after the configuration migrations (4123 before migration 404).
/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 11631;
const RECEIVE_SIZE: usize = 1024;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
/// Size of the packet: eleven 32-bit floats, 4-byte packing.
const PACKET_LEN: usize = 44;

/// One Iron Rebellion packet (the fields after the leading padding).
#[derive(Debug, Clone, Copy, PartialEq)]
struct IronRebellionPacket {
    roll: f64,
    pitch: f64,
    yaw: f64,
    surge: f64,
    sway: f64,
    heave: f64,
    speed: f64,
}

impl IronRebellionPacket {
    /// Decodes the first 44 bytes of a datagram. For a datagram shorter than 44 bytes
    /// the missing
    /// bytes read zero.
    fn copy_from(data: &[u8]) -> Self {
        let mut raw = [0u8; PACKET_LEN];
        let n = data.len().min(PACKET_LEN);
        raw[..n].copy_from_slice(&data[..n]);
        let f = Fields::new(&raw);
        let at = |o: usize| f.f32(o).unwrap_or(0.0);
        IronRebellionPacket {
            roll: at(4),
            pitch: at(8),
            yaw: at(12),
            surge: at(16),
            sway: at(20),
            heave: at(24),
            speed: at(28),
        }
    }
}

/// The Iron Rebellion listener. This listener does not
/// forward datagrams.
pub struct IronRebellionSource {
    receiver: Receiver,
    native_setup: bool,
    datagram: Vec<u8>,
    packet: Option<IronRebellionPacket>,
}

impl IronRebellionSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::without_forwarding_imported(net, Endpoint::any(0, RECEIVE_TIMEOUT), RECEIVE_SIZE, port),
            native_setup: true,
            datagram: Vec::new(),
            packet: None,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::any(settings.port, RECEIVE_TIMEOUT);
        IronRebellionSource {
            receiver: Receiver::without_forwarding(net, endpoint, RECEIVE_SIZE),
            native_setup: false,
            datagram: Vec::new(),
            packet: None,
        }
    }

    fn check_status_with_report(&mut self, mut report: impl FnMut(String)) -> bool {
        match self.receiver.receive_once() {
            Ok(data) => {
                self.datagram = data;
                !self.datagram.is_empty()
            }
            Err(ReceiveFailure::Bind(error)) => {
                let stage = if socket_creation_failed(&error) { "Socket" } else { "Bind" };
                report(format!("{stage} Error: {error}"));
                false
            }
            Err(ReceiveFailure::Receive(_)) => false,
        }
    }

    /// Writes the channels: 0 for everything until a packet arrived. Where a NaN or infinite
    /// speed has no integer value the tick ends after `lateral_velocity` ([`publish_speed_unreadable`]; `car_name` and
    /// `lateral_velocity` are not mapped and read 0).
    fn write_channels(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let Some(p) = self.packet else {
            zero_listener_channels(t);
            return Ok(());
        };
        let Some(speed) = trunc(p.speed) else {
            return publish_speed_unreadable(t, Some(String::new()), 0.0);
        };
        zero_listener_channels(t);
        t.roll = -p.roll;
        t.pitch = -p.pitch;
        t.yaw = p.yaw;
        t.longitudinal_acceleration = p.surge;
        t.lateral_acceleration = p.sway;
        t.vertical_acceleration = p.heave;
        t.speed = speed;
        Ok(())
    }
}

impl Source for IronRebellionSource {
    fn id(&self) -> &'static str {
        "ironrebellion"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let Some(p) = self.packet else { return Some(LegacyValue::Int(0)) };
        let value = match name {
            "speed" => return int_text(p.speed).map(LegacyValue::Str),
            "roll" => -p.roll,
            "pitch" => -p.pitch,
            "yaw" => p.yaw,
            "longitudinal_acceleration" => p.surge,
            "lateral_acceleration" => p.sway,
            "vertical_acceleration" => p.heave,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    /// The liveness test: true for any non-empty datagram.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let native_setup = self.native_setup;
        Ok(self.check_status_with_report(|message| {
            if native_setup {
                tracing::debug!("{message}");
            }
        }))
    }

    /// The receive step.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        self.packet = Some(IronRebellionPacket::copy_from(&self.datagram));
        Ok(true)
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
    use crate::udp_racing::forward::test_forwarder;
    use crate::udp_racing::support::net::fake::{FakeNetwork, ForwardSink};

    #[test]
    fn setup_reports_creation_and_bind_failures_but_not_receive_timeout() {
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
        for (creation, expected) in [(true, "Socket Error: allocation denied"), (false, "Bind Error: bind denied")] {
            let mut source = IronRebellionSource::with_network(
                ListenerSettings::with_port(DEFAULT_PORT),
                Box::new(Failure(creation)),
            );
            let mut reports = Vec::new();
            assert!(!source.check_status_with_report(|message| reports.push(message)));
            assert_eq!(reports, [expected]);
        }
        let mut source = IronRebellionSource::with_network(
            ListenerSettings::with_port(DEFAULT_PORT),
            Box::new(FakeNetwork::default()),
        );
        assert!(!source.check_status_with_report(|_| panic!("receive exceptions are silent")));
    }

    #[test]
    fn native_port_retry_keeps_packet_and_never_forwards() {
        use super::super::forward::{Forwarder, PortForwarding};
        use super::super::parameters::Parameters;
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        };
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native cached port must reach bind_imported")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint.bind.ip(), std::net::Ipv4Addr::UNSPECIFIED);
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
        let old_data: Vec<_> = [0.0f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 0.0, 0.0, 0.0]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let mut new_data = old_data.clone();
        new_data[28..32].copy_from_slice(&11.0f32.to_le_bytes());
        let good = FakeNetwork::default();
        good.push_timeout(6001);
        good.push(6001, &new_data);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = IronRebellionSource::with_imported_network(
            ImportedPort::Valid(6001),
            Box::new(Staged(calls.clone(), good)),
        );
        source.datagram = old_data.clone();
        source.packet = Some(IronRebellionPacket::copy_from(&old_data));
        let parameters = Arc::new(Parameters::default());
        parameters.set_listener_running("ironrebellion");
        let forwarding = Arc::new(PortForwarding::default());
        let forwarded = Arc::new(AtomicUsize::new(0));
        forwarding.set_reader({
            let forwarded = forwarded.clone();
            Arc::new(move |_| {
                forwarded.fetch_add(1, Ordering::Relaxed);
                None
            })
        });
        source.receiver.set_forwarder(Forwarder::with(parameters, forwarding));
        let mut reports = Vec::new();
        for _ in 0..4 {
            assert!(!source.check_status_with_report(|message| reports.push(message)));
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("7".into())));
        }
        assert_eq!(reports, ["Socket Error: create failed", "Bind Error: bind failed", "Bind Error: timeout failed"]);
        assert!(source.check_status_with_report(|_| panic!("successful receive")));
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("11".into())));
        assert_eq!(forwarded.load(Ordering::Relaxed), 0);
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = IronRebellionSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        let mut reports = Vec::new();
        assert!(!source.check_status_with_report(|message| reports.push(message)));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
        assert_eq!(reports, ["Bind Error: bind(): port must be 0-65535."]);
    }

    #[test]
    fn decodes_and_maps() {
        // Receive step and channels; bind 0.0.0.0, no forwarding
        let net = FakeNetwork::default();
        let sink = ForwardSink::new();
        let mut s = IronRebellionSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        s.receiver.set_forwarder(test_forwarder("ironrebellion", &[sink.target()]).0);
        let values = [0.0f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.9, 8.0, 9.0, 10.0];
        net.push(DEFAULT_PORT, &values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        assert!(s.is_running().unwrap());
        assert!(s.is_running().is_ok());
        assert_eq!(net.bound()[0].bind, "0.0.0.0:11631".parse().unwrap());
        assert!(sink.received().is_empty());
    }

    #[test]
    fn channel_mapping() {
        // Channels: roll/pitch negated, surge/sway/heave, speed as an integer
        let p = IronRebellionPacket::copy_from(
            &[0.0f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.9, 0.0, 0.0, 0.0]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let net = FakeNetwork::default();
        let mut s = IronRebellionSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net));
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Int(0)));
        s.packet = Some(p);
        let mut t = Telemetry::off();
        s.write_channels(&mut t).unwrap();
        assert_eq!((t.roll, t.pitch, t.yaw), (-1.0, -2.0, 3.0));
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.vertical_acceleration), (4.0, 5.0, 6.0));
        assert_eq!(t.speed, 7.0);
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Str("7".into())));
        assert_eq!(s.native_channel("pitch"), Some(LegacyValue::Float(-2.0)));
        assert_eq!(s.native_channel("gear"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn empty_datagram_is_not_running() {
        // Liveness: an empty datagram answers false
        let net = FakeNetwork::default();
        let mut s = IronRebellionSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[]);
        assert!(!s.is_running().unwrap());
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // A NaN speed has no integer value; the dispatcher's top-speed update
        // does not handle that, so only car_name (0), track_loc (0) and lateral_velocity (0) are stored
        let p = IronRebellionPacket::copy_from(
            &[0.0f32, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, f32::NAN, 0.0, 0.0, 0.0]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        let mut s = IronRebellionSource::with_network(
            ListenerSettings::with_port(DEFAULT_PORT),
            Box::new(FakeNetwork::default()),
        );
        s.packet = Some(p);
        let before = Telemetry { roll: 5.0, speed: 9.0, lateral_velocity: 2.0, ..Telemetry::off() };
        let mut t = before.clone();
        assert!(s.write_channels(&mut t).is_err(), "getMaxSpeed raises out of the dispatcher");
        assert_eq!(t, Telemetry { lateral_velocity: 0.0, ..before });
        assert_eq!(s.native_channel("speed"), None);
        assert!(s.channel_read_raised("speed"));
        assert_eq!(s.native_channel("pitch"), Some(LegacyValue::Float(-2.0)));
    }
}

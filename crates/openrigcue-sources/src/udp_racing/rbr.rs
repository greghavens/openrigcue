//! Richard Burns Rally with the NGP plugin's UDP telemetry (listener `rbr`).

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::zero_listener_channels;
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::{native_int, trunc};
use super::support::receiver::{ReceiveFailure, Receiver};
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 6776;
const RECEIVE_SIZE: usize = 952;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
/// Size of the byte-packed telemetry structure (its tire block has seven segments).
const TELEMETRY_DATA_LEN: usize = 632;
const KMH_TO_MS: f64 = 0.277778;
const MAX_RPM: f64 = 10000.0;

/// The telemetry fields the listener copies (all before the first tire block, so their offsets
/// equal NGP's).
#[derive(Debug, Clone, Copy, PartialEq)]
struct RbrTelemetry {
    steering: f64,
    gear: i32,
    /// km/h.
    speed: f64,
    roll: f64,
    pitch: f64,
    yaw: f64,
    /// Surge, sway, heave.
    velocities: [f64; 3],
    accelerations: [f64; 3],
    rpm: f64,
}

impl RbrTelemetry {
    /// Decodes a datagram of at least the structure size.
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        f.bytes(0, TELEMETRY_DATA_LEN)?;
        Ok(RbrTelemetry {
            steering: f.f32(24)?,
            gear: f.i32(44)?,
            speed: f.f32(60)?,
            roll: f.f32(76)?,
            pitch: f.f32(80)?,
            yaw: f.f32(84)?,
            velocities: [f.f32(88)?, f.f32(92)?, f.f32(96)?],
            accelerations: [f.f32(112)?, f.f32(116)?, f.f32(120)?],
            rpm: f.f32(136)?,
        })
    }
}

/// The Richard Burns Rally listener.
pub struct RbrSource {
    receiver: Receiver,
    native_socket: bool,
    socket_assigned: bool,
    datagram: Vec<u8>,
    telemetry: Option<RbrTelemetry>,
    /// The stored `rpm`: written last by the receive step as the integer engine rpm, which fails for a NaN or
    /// infinite rpm and leaves the previous entry (none before the first finite one).
    rpm: Option<f64>,
}

impl RbrSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::with_imported_port(net, Endpoint::loopback(0, RECEIVE_TIMEOUT), RECEIVE_SIZE, port),
            native_socket: true,
            socket_assigned: false,
            datagram: Vec::new(),
            telemetry: None,
            rpm: None,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, RECEIVE_TIMEOUT);
        let receiver = Receiver::new(net, endpoint, RECEIVE_SIZE);
        RbrSource {
            receiver,
            native_socket: false,
            socket_assigned: false,
            datagram: Vec::new(),
            telemetry: None,
            rpm: None,
        }
    }

    /// Writes the channels from the values the receive step stores (none, hence 0, until the first packet).
    fn write_channels(&self, t: &mut Telemetry) {
        zero_listener_channels(t);
        let Some(r) = self.telemetry else { return };
        t.yaw = r.yaw;
        t.roll = r.roll;
        t.pitch = r.pitch;
        t.vertical_acceleration = r.accelerations[2] * 10.0;
        t.lateral_acceleration = r.accelerations[1];
        t.longitudinal_acceleration = r.accelerations[0];
        t.lateral_velocity = r.velocities[1];
        t.longitudinal_velocity = r.velocities[0];
        t.vertical_velocity = r.velocities[2];
        t.max_rpm = MAX_RPM;
        t.steering_wheel_angle = r.steering;
        t.gear = f64::from(r.gear) - 1.0;
        t.speed = r.speed * KMH_TO_MS;
        t.rpm = self.rpm.unwrap_or(0.0);
    }
}

impl Source for RbrSource {
    fn id(&self) -> &'static str {
        "rbr"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let Some(r) = self.telemetry else { return Some(LegacyValue::Int(0)) };
        let value = match name {
            "rpm" => return Some(self.rpm.and_then(native_int).unwrap_or(LegacyValue::Int(0))),
            "max_rpm" => return Some(LegacyValue::Int(10000)),
            "gear" => return Some(LegacyValue::Int(i64::from(r.gear) - 1)),
            "yaw" => r.yaw,
            "roll" => r.roll,
            "pitch" => r.pitch,
            "vertical_acceleration" => r.accelerations[2] * 10.0,
            "lateral_acceleration" => r.accelerations[1],
            "longitudinal_acceleration" => r.accelerations[0],
            "lateral_velocity" => r.velocities[1],
            "longitudinal_velocity" => r.velocities[0],
            "vertical_velocity" => r.velocities[2],
            "steering_wheel_angle" => r.steering,
            "speed" => r.speed * KMH_TO_MS,
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    /// The liveness test: any datagram.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_once() {
            Ok(data) => {
                self.socket_assigned = true;
                self.datagram = data;
                true
            }
            Err(ReceiveFailure::Bind(error)) if self.native_socket && socket_creation_failed(&error) => {
                if !self.socket_assigned {
                    return Err(
                        DispatcherEnds::new("AttributeError: 'NoneType' object has no attribute 'close'").into()
                    );
                }
                self.receiver.close();
                false
            }
            Err(_) => {
                self.socket_assigned = true;
                false
            }
        };
        Ok(running)
    }

    /// The receive step.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let parsed = RbrTelemetry::parse(&self.datagram).ok();
        if let Some(packet) = parsed {
            self.telemetry = Some(packet);
            // RPM is the last value written; conversion failure keeps the
            // completed prefix and old RPM, but the receive step still answers false.
            let rpm = trunc(packet.rpm);
            if rpm.is_none() {
                // Prints "Error" and the failure text: a float that has no integer value.
                let what = if packet.rpm.is_nan() { "NaN" } else { "infinity" };
                crate::printed::line(format!("Error cannot convert float {what} to integer"));
            }
            self.rpm = rpm.or(self.rpm);
            return Ok(rpm.is_some());
        }
        // The same message, for a datagram too short to decode.
        crate::printed::line(format!(
            "Error Buffer size too small ({} instead of at least {TELEMETRY_DATA_LEN} bytes)",
            self.datagram.len()
        ));
        Ok(false)
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

    #[test]
    fn first_native_creation_failure_raises_the_original_none_close_error() {
        struct Failure;
        impl Network for Failure {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn super::super::support::net::Datagrams>> {
                Err(super::super::socket::socket_creation_error(std::io::Error::other("creation denied")))
            }
        }
        let mut source = RbrSource::with_imported_network(ImportedPort::Valid(DEFAULT_PORT), Box::new(Failure));
        let error = source.is_running().expect_err("the original catch calls close on its initial None");
        assert!(super::super::unhandled::ends_dispatcher(&error));
        assert!(error.to_string().contains("'NoneType' object has no attribute 'close'"));
    }

    #[test]
    fn native_bind_then_creation_failure_keeps_existing_socket_state_and_telemetry() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("imported bind")
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
                    1 => Err(io::Error::other("bind denied")),
                    2 => Err(super::super::socket::socket_creation_error(io::Error::other("creation denied"))),
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
        let old_packet = RbrTelemetry::parse(&old_data).unwrap();
        let mut new_data = old_data.clone();
        new_data[136..140].copy_from_slice(&7200.0f32.to_le_bytes());
        let ready = FakeNetwork::default();
        ready.push_timeout(6001);
        ready.push(6001, &new_data);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source =
            RbrSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), ready)));
        source.datagram = old_data.clone();
        source.telemetry = Some(old_packet);
        source.rpm = Some(6500.0);
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert!(source.socket_assigned);
            assert_eq!(source.datagram, old_data);
            assert_eq!(source.telemetry, Some(old_packet));
            assert_eq!(source.rpm, Some(6500.0));
        }
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(source.rpm, Some(7200.0));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = RbrSource::with_imported_network(
            ImportedPort::OutOfRange,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap());
        assert!(source.socket_assigned);
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange]);
    }

    #[test]
    fn native_first_creation_error_stops_the_actual_dispatcher_prefix() {
        use crate::derive::{
            AttitudeSource, DamsMode, DamsSettings, DispatchSettings, Dispatcher, GameDirectory, IntellituneReference,
            ProfileSettings, RefreshRates, RuntimeInputs, ScanHit, SourceListener, SpeedUnit, TickInput,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        struct Games;
        impl GameDirectory for Games {
            fn is_flying(&self, _: &str) -> bool {
                false
            }
            fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
                None
            }
        }
        struct Failure;
        impl Network for Failure {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn super::super::support::net::Datagrams>> {
                Err(super::super::socket::socket_creation_error(std::io::Error::other("creation denied")))
            }
        }
        struct Later(Arc<AtomicUsize>);
        impl Source for Later {
            fn id(&self) -> &'static str {
                "later"
            }
            fn is_running(&mut self) -> Result<bool, SourceError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(false)
            }
            fn poll(&mut self, _: &mut Telemetry) -> Result<bool, SourceError> {
                panic!("later receive")
            }
        }
        let source = RbrSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Failure));
        let later = Arc::new(AtomicUsize::new(0));
        let mut dispatcher = Dispatcher::new(vec![
            Box::new(SourceListener::new(source)),
            Box::new(SourceListener::new(Later(later.clone()))),
        ]);
        let settings = DispatchSettings {
            dams: DamsSettings {
                mode: DamsMode::Off,
                factor: Some(1.0),
                front_lever: 0.0,
                rear_lever: 1.0,
                steering_ratio: Some(1.0),
            },
            refresh_rates: RefreshRates { shakebox: 10, intellibox: 60, motionbox: 30 },
            native_refresh_rates: None,
            speed_unit: SpeedUnit::Kmh,
            native_speed_unit: None,
            show_all_channels: false,
        };
        let profile = ProfileSettings {
            default_max_speed_kmh: 200.0,
            gear_shift_percent: 96.0,
            pitch_source: AttitudeSource::Angle,
            roll_source: AttitudeSource::Angle,
            native: None,
        };
        let runtime = RuntimeInputs::default();
        let hits = [
            ScanHit { game_id: "rbr".into(), listener_id: "rbr".into() },
            ScanHit { game_id: "later".into(), listener_id: "later".into() },
        ];
        let input = TickInput { hits: &hits, settings: &settings, profile: &profile, runtime: &runtime, games: &Games };
        let outcome = dispatcher.tick(&input);
        assert!(dispatcher.has_ended());
        assert_eq!(later.load(Ordering::Relaxed), 0);
        assert_eq!(outcome.events, [crate::derive::DispatchEvent::GameRunning("rbr".into())]);
        assert!(dispatcher.telemetry_writes().is_empty());
        assert_eq!(outcome.sleep, Duration::ZERO);
        dispatcher.tick(&input);
        assert_eq!(later.load(Ordering::Relaxed), 0);
    }

    /// An NGP datagram (664 bytes) with the fields the listener reads.
    fn datagram() -> Vec<u8> {
        let mut d = vec![0u8; 664];
        let mut put = |o: usize, v: f32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(24, -0.5);
        put(60, 100.0);
        put(76, 0.01);
        put(80, 0.02);
        put(84, 1.5);
        put(88, 27.0);
        put(92, 1.0);
        put(96, -0.25);
        put(112, 3.0);
        put(116, -2.0);
        put(120, 0.5);
        put(136, 6500.7);
        d[44..48].copy_from_slice(&4i32.to_le_bytes());
        d
    }

    #[test]
    fn channels() {
        // Receive step mapping and channels
        let net = FakeNetwork::default();
        let mut s = RbrSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &datagram());
        assert!(s.is_running().unwrap());
        assert_eq!(net.bound()[0].bind, "127.0.0.1:6776".parse().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.yaw, t.steering_wheel_angle, t.gear, t.rpm, t.max_rpm), (1.5, -0.5, 3.0, 6500.0, 10000.0));
        assert_eq!(t.speed, 100.0 * 0.277778);
        assert_eq!((t.vertical_acceleration, t.lateral_acceleration, t.longitudinal_acceleration), (5.0, -2.0, 3.0));
        assert_eq!((t.lateral_velocity, t.longitudinal_velocity, t.vertical_velocity), (1.0, 27.0, -0.25));
    }

    #[test]
    fn non_finite_rpm_keeps_the_listeners_last_rpm() {
        // Receive step: the integer rpm is written last and fails for NaN, leaving
        // the previous rpm; a channel read before any rpm answers 0
        let net = FakeNetwork::default();
        let mut s = RbrSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut nan = datagram();
        nan[136..140].copy_from_slice(&f32::NAN.to_le_bytes());
        nan[24..28].copy_from_slice(&(-0.75f32).to_le_bytes());
        net.push(DEFAULT_PORT, &nan);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry { rpm: 900.0, ..Telemetry::off() };
        crate::printed::take();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(crate::printed::take(), ["Error cannot convert float NaN to integer"], "print('Error', str(e))");
        assert_eq!((t.rpm, t.gear), (0.0, 3.0), "no rpm in the dict yet; the other keys were assigned");
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("max_rpm"), Some(LegacyValue::Int(10000)));
        assert_eq!(s.native_channel("gear"), Some(LegacyValue::Int(3)));
        assert_eq!(s.native_channel("steering_wheel_angle"), Some(LegacyValue::Float(-0.75)));
        assert!(!s.channel_read_raised("rpm"), "the missing dictionary key is caught by getChannel");
        net.push(DEFAULT_PORT, &datagram());
        assert!(s.is_running().unwrap());
        assert!(s.poll(&mut t).unwrap());
        net.push(DEFAULT_PORT, &nan);
        assert!(s.is_running().unwrap());
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 6500.0);
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Int(6500)));
        assert_eq!(s.native_channel("steering_wheel_angle"), Some(LegacyValue::Float(-0.75)));
    }

    #[test]
    fn short_datagram_reads_zero_until_a_full_one() {
        // Decoding needs 632 bytes; nothing is stored -> 0
        let net = FakeNetwork::default();
        let mut s = RbrSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[0u8; 631]);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        crate::printed::take();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(crate::printed::take(), ["Error Buffer size too small (631 instead of at least 632 bytes)"]);
        assert_eq!(t.gear, 0.0);
        assert_eq!(s.native_channel("gear"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("speed"), Some(LegacyValue::Int(0)));
    }
}

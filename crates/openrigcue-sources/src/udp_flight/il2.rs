//! IL-2 Sturmovik Great Battles: the game's "motion device" and "telemetry device" UDP streams, both on one port.
//!
//! A 44-byte datagram is a motion packet: an 8-byte header, then nine little-endian `f32`: yaw, pitch, roll (rad),
//! spin x/y/z (rad/s), acceleration x/y/z. Anything else is parsed as a telemetry-device packet (indicators and
//! events), from which speed, rpm and the aircraft name are taken.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::channels::{assign, clear_listener_channels, AngularRates, FlightSource};
use super::python::{fold_roll, rad_to_deg};
use super::transport::UdpConfig;
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::net::{Endpoint, Network, SystemNetwork};
use crate::udp_racing::socket::ImportedPort;
use crate::{Source, SourceError};

/// Listener id.
pub const ID: &str = "il2";
/// Default port (`NETWORK_PORT.il2`).
pub const DEFAULT_PORT: u16 = 4321;
const TIMEOUT: Duration = Duration::from_secs(1);
const MAX_DATAGRAM: usize = 2048;
/// Length that identifies a motion-device packet.
const MOTION_PACKET_LEN: usize = 44;
const MOTION_HEADER_LEN: usize = 8;
/// Bytes skipped before the indicator count of a telemetry-device packet.
const TELEMETRY_HEADER_LEN: usize = 10;
/// Indicator ids taken from a telemetry-device packet.
const INDICATOR_RPM: u16 = 0;
const INDICATOR_SPEED: u16 = 6;
/// Event id carrying the aircraft name ("set_focus").
const EVENT_SET_FOCUS: u16 = 0;
/// Aircraft name treated as "no name".
const UNKNOWN_AIRCRAFT: &str = "Unknown";
/// The `max_rpm` value, set on every processed datagram.
const MAX_RPM: f64 = 10000.0;

/// What the receive thread has stored so far.
#[derive(Debug, Clone, Default, PartialEq)]
struct Il2Values {
    max_rpm: Option<f64>,
    lateral_acceleration: Option<f64>,
    vertical_acceleration: Option<f64>,
    longitudinal_acceleration: Option<f64>,
    yaw: Option<f64>,
    pitch: Option<f64>,
    roll: Option<f64>,
    yaw_rate: Option<f64>,
    pitch_rate: Option<f64>,
    roll_rate: Option<f64>,
    speed: Option<f64>,
    rpm: Option<f64>,
    aircraft: Option<String>,
}

impl Il2Values {
    /// The compatibility parser uses the same ordered assignments as the background receiver.
    #[cfg(test)]
    fn process(&mut self, datagram: &[u8]) -> bool {
        process_with(datagram, |change| change.apply(self))
    }

    /// A channel read: the stored value, or integer zero.
    fn native_channel(&self, name: &str) -> LegacyValue {
        let value = match name {
            "max_rpm" => return LegacyValue::Int(if self.max_rpm.is_some() { 10000 } else { 0 }),
            "car_name" => return self.aircraft.clone().map_or(LegacyValue::Int(0), LegacyValue::Str),
            "lateral_acceleration" => self.lateral_acceleration,
            "vertical_acceleration" => self.vertical_acceleration,
            "longitudinal_acceleration" => self.longitudinal_acceleration,
            "yaw" => self.yaw,
            "pitch" => self.pitch,
            "roll" => self.roll,
            "yaw_rate" => self.yaw_rate,
            "pitch_rate" => self.pitch_rate,
            "roll_rate" => self.roll_rate,
            "speed" => self.speed,
            "rpm" => self.rpm,
            _ => None,
        };
        value.map_or(LegacyValue::Int(0), LegacyValue::Float)
    }

    /// Numeric projection of the stored channels (0 when absent).
    fn write(&self, t: &mut Telemetry) {
        assign(&mut t.max_rpm, self.max_rpm);
        assign(&mut t.lateral_acceleration, self.lateral_acceleration);
        assign(&mut t.vertical_acceleration, self.vertical_acceleration);
        assign(&mut t.longitudinal_acceleration, self.longitudinal_acceleration);
        assign(&mut t.yaw, self.yaw);
        assign(&mut t.pitch, self.pitch);
        assign(&mut t.roll, self.roll);
        assign(&mut t.speed, self.speed);
        assign(&mut t.rpm, self.rpm);
        if let Some(aircraft) = &self.aircraft {
            t.vehicle.clone_from(aircraft);
        }
    }

    /// The `pitch_rate` / `roll_rate` / `yaw_rate` channels.
    fn rates(&self) -> AngularRates {
        AngularRates {
            pitch: self.pitch_rate.unwrap_or(0.0),
            roll: self.roll_rate.unwrap_or(0.0),
            yaw: self.yaw_rate.unwrap_or(0.0),
        }
    }
}

/// Speed, rpm and aircraft name of one telemetry-device packet.
#[derive(Debug, PartialEq)]
struct TelemetryReading {
    speed: f64,
    rpm: f64,
    aircraft: String,
}

/// Individual value assignments of one processed datagram.
enum ValueChange {
    Maximum,
    Float(&'static str, f64),
    Aircraft(String),
}

impl ValueChange {
    fn apply(self, values: &mut Il2Values) {
        match self {
            Self::Maximum => values.max_rpm = Some(MAX_RPM),
            Self::Aircraft(name) => values.aircraft = Some(name),
            Self::Float(name, value) => {
                let field = match name {
                    "lateral_acceleration" => &mut values.lateral_acceleration,
                    "vertical_acceleration" => &mut values.vertical_acceleration,
                    "longitudinal_acceleration" => &mut values.longitudinal_acceleration,
                    "yaw" => &mut values.yaw,
                    "pitch" => &mut values.pitch,
                    "yaw_rate" => &mut values.yaw_rate,
                    "pitch_rate" => &mut values.pitch_rate,
                    "roll_rate" => &mut values.roll_rate,
                    "roll" => &mut values.roll,
                    "speed" => &mut values.speed,
                    "rpm" => &mut values.rpm,
                    _ => unreachable!("known IL-2 assignment"),
                };
                *field = Some(value);
            }
        }
    }
}

fn process_with(datagram: &[u8], mut publish: impl FnMut(ValueChange)) -> bool {
    publish(ValueChange::Maximum);
    if datagram.len() == MOTION_PACKET_LEN {
        let p: [f64; 9] = std::array::from_fn(|i| {
            let at = MOTION_HEADER_LEN + 4 * i;
            f64::from(f32::from_le_bytes(datagram[at..at + 4].try_into().expect("4 bytes")))
        });
        for (name, value) in [
            ("lateral_acceleration", p[7]),
            ("vertical_acceleration", p[8]),
            ("longitudinal_acceleration", p[6]),
            ("yaw", rad_to_deg(p[0])),
            ("pitch", -rad_to_deg(p[1])),
            ("yaw_rate", rad_to_deg(p[5])),
            ("pitch_rate", -rad_to_deg(p[4])),
            ("roll_rate", -rad_to_deg(p[3])),
            ("roll", fold_roll(rad_to_deg(p[2]))),
        ] {
            publish(ValueChange::Float(name, value));
        }
        return true;
    }
    let Some(reading) = parse_telemetry_device(datagram) else { return false };
    publish(ValueChange::Float("speed", reading.speed));
    publish(ValueChange::Float("rpm", reading.rpm));
    if reading.aircraft != UNKNOWN_AIRCRAFT {
        publish(ValueChange::Aircraft(reading.aircraft));
    }
    true
}

/// Byte reader: seeking past the end is allowed, reads return what is left.
struct Stream<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Stream<'a> {
    fn skip(&mut self, count: usize) {
        self.position += count;
    }

    /// Up to `count` bytes.
    fn read(&mut self, count: usize) -> &'a [u8] {
        let start = self.position.min(self.bytes.len());
        let end = self.position.saturating_add(count).min(self.bytes.len());
        self.position = start + (end - start);
        &self.bytes[start..end]
    }

    /// Exactly `count` bytes, as decoding a fixed-size field requires.
    fn read_exact<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.read(N).try_into().ok()
    }
}

/// Parses a telemetry-device packet and its events. `None` for a
/// truncated field, a non-ASCII name, or a packet without the speed or rpm indicator.
fn parse_telemetry_device(bytes: &[u8]) -> Option<TelemetryReading> {
    let mut stream = Stream { bytes, position: 0 };
    stream.skip(TELEMETRY_HEADER_LEN);
    let indicator_count = i8::from_le_bytes(stream.read_exact::<1>()?);
    let (mut speed, mut rpm) = (None, None);
    for _ in 0..indicator_count.max(0) {
        let id = stream.read(2).iter().rev().fold(0u16, |id, &b| id << 8 | u16::from(b));
        let [value_count] = stream.read_exact::<1>()?;
        for _ in 0..value_count {
            let value = f64::from(f32::from_le_bytes(stream.read_exact::<4>()?));
            match id {
                INDICATOR_RPM => rpm = Some(value),
                INDICATOR_SPEED => speed = Some(value),
                _ => {}
            }
        }
    }
    let [event_count] = stream.read_exact::<1>()?;
    let mut aircraft = None;
    for _ in 0..event_count {
        let header = stream.read_exact::<3>()?;
        let event_id = u16::from_le_bytes([header[0], header[1]]);
        if event_id == EVENT_SET_FOCUS {
            let [name_len] = stream.read_exact::<1>()?;
            let name = stream.read(usize::from(name_len));
            if !name.is_ascii() {
                return None;
            }
            aircraft = Some(String::from_utf8_lossy(name).into_owned());
        } else {
            stream.skip(usize::from(header[2]));
        }
    }
    Some(TelemetryReading {
        speed: speed?,
        rpm: rpm?,
        aircraft: aircraft.unwrap_or_else(|| UNKNOWN_AIRCRAFT.to_owned()),
    })
}

/// State shared with the receive thread.
#[derive(Debug, Default)]
struct Shared {
    connected: bool,
    values: Il2Values,
}

/// The IL-2 listener (`il2`): a background thread bound for the source's whole lifetime.
pub struct Il2Source {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    rates: AngularRates,
}

impl Il2Source {
    /// Starts the receive thread.
    pub fn new(config: UdpConfig) -> Self {
        Self::from_imported_port(ImportedPort::Valid(config.port))
    }

    /// Start the receive thread without narrowing its cached integer port.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let forwarder = Arc::new(Forwarder::default());
        let stop = Arc::new(AtomicBool::new(false));
        let receiver = Receiver { port, forwarder, shared: shared.clone(), stop: stop.clone() };
        let thread = thread::Builder::new().name("il2_main_loop".into()).spawn(move || receiver.run()).ok();
        Il2Source { shared, stop, thread, rates: AngularRates::default() }
    }

    fn connected(&self) -> bool {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).connected
    }
}

impl Source for Il2Source {
    fn id(&self) -> &'static str {
        ID
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        Some(self.shared.lock().unwrap_or_else(PoisonError::into_inner).values.native_channel(name))
    }

    /// The liveness test: whether the last receive got a datagram.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.connected())
    }

    /// The receive step reads only the receiver's connection flag.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.connected())
    }

    /// Numeric compatibility view of the background receiver's current channels.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let (connected, values) = {
            let shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            (shared.connected, shared.values.clone())
        };
        clear_listener_channels(telemetry);
        values.write(telemetry);
        self.rates = values.rates();
        Ok(connected)
    }
}

impl FlightSource for Il2Source {
    fn angular_rates(&self) -> AngularRates {
        self.rates
    }
}

impl Drop for Il2Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The receive loop.
struct Receiver {
    port: ImportedPort,
    forwarder: Arc<Forwarder>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

impl Receiver {
    fn run(self) {
        self.run_with(&mut SystemNetwork);
    }

    fn run_with(&self, network: &mut dyn Network) {
        let endpoint = Endpoint::new(Ipv4Addr::LOCALHOST, 0, TIMEOUT);
        let mut socket = match network.bind_imported(&endpoint, self.port) {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(%error, "il2 listener could not bind");
                return;
            }
        };
        while !self.stop.load(Ordering::Relaxed) {
            self.on_receive(socket.receive(MAX_DATAGRAM).map(|(data, _)| data));
        }
    }

    fn on_receive(&self, received: std::io::Result<Vec<u8>>) {
        match received {
            Ok(datagram) => {
                if datagram.len() == MOTION_PACKET_LEN {
                    self.forwarder.forward(&datagram);
                }
                // The connected flag and every assignment of one datagram are published
                // together: a getter sees none or all of one datagram.
                let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
                shared.connected = true;
                process_with(&datagram, |change| change.apply(&mut shared.values));
            }
            Err(_) => self.shared.lock().unwrap_or_else(PoisonError::into_inner).connected = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_flight::transport::testing::{free_port, Sender};

    fn receiver(port: ImportedPort) -> Receiver {
        Receiver {
            port,
            forwarder: Arc::new(Forwarder::default()),
            shared: Arc::new(Mutex::new(Shared::default())),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn native_worker_setup_failure_does_not_retry_or_publish() {
        use crate::udp_racing::net::Datagrams;
        use crate::udp_racing::socket::socket_creation_error;
        use std::io;
        struct Fail {
            stage: &'static str,
            calls: Vec<&'static str>,
        }
        impl Network for Fail {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native port must reach bind_imported")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint, Endpoint::loopback(0, Duration::from_secs(1)));
                self.calls.push("create");
                if self.stage == "create" {
                    return Err(socket_creation_error(io::Error::other("create failed")));
                }
                self.calls.push("bind");
                let _ = port.for_bind()?;
                if self.stage == "bind" {
                    return Err(io::Error::other("bind failed"));
                }
                self.calls.push("timeout");
                Err(io::Error::other("timeout failed"))
            }
        }
        for (stage, port, expected) in [
            ("create", ImportedPort::OutOfRange, vec!["create"]),
            ("range", ImportedPort::OutOfRange, vec!["create", "bind"]),
            ("bind", ImportedPort::Valid(6001), vec!["create", "bind"]),
            ("timeout", ImportedPort::Valid(6001), vec!["create", "bind", "timeout"]),
        ] {
            let receiver = receiver(port);
            let previous = receiver.shared.lock().unwrap().values.clone();
            let mut network = Fail { stage, calls: Vec::new() };
            receiver.run_with(&mut network);
            assert_eq!(network.calls, expected);
            let shared = receiver.shared.lock().unwrap();
            assert!(!shared.connected);
            assert_eq!(shared.values, previous);
        }
    }

    #[test]
    fn native_worker_retains_one_socket_and_recovers_after_receive_failure() {
        use crate::udp_racing::net::Datagrams;
        use std::io;
        use std::net::SocketAddr;
        struct Script {
            stop: Arc<AtomicBool>,
            shared: Arc<Mutex<Shared>>,
            receives: usize,
        }
        impl Datagrams for Script {
            fn receive(&mut self, max_len: usize) -> io::Result<(Vec<u8>, SocketAddr)> {
                assert_eq!(max_len, 2048);
                self.receives += 1;
                match self.receives {
                    1 => Ok((
                        motion_packet([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 3.0, 0.0, 0.0]),
                        SocketAddr::from(([127, 0, 0, 1], 6001)),
                    )),
                    2 => {
                        let shared = self.shared.lock().unwrap();
                        assert!(shared.connected);
                        assert_eq!(shared.values.longitudinal_acceleration, Some(3.0));
                        Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
                    }
                    3 => {
                        let shared = self.shared.lock().unwrap();
                        assert!(!shared.connected);
                        assert_eq!(shared.values.longitudinal_acceleration, Some(3.0));
                        self.stop.store(true, Ordering::Relaxed);
                        Ok((
                            motion_packet([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 7.0, 0.0, 0.0]),
                            SocketAddr::from(([127, 0, 0, 1], 6001)),
                        ))
                    }
                    _ => panic!("worker should stop"),
                }
            }
            fn send_to(&mut self, _: &[u8], _: SocketAddr) -> io::Result<()> {
                panic!("no sends")
            }
            fn try_clone(&self) -> io::Result<Box<dyn Datagrams>> {
                panic!("no clones")
            }
        }
        struct OneSocket {
            socket: Option<Script>,
            opens: usize,
        }
        impl Network for OneSocket {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("use imported")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint, Endpoint::loopback(0, Duration::from_secs(1)));
                assert_eq!(port, ImportedPort::Valid(6001));
                self.opens += 1;
                Ok(Box::new(self.socket.take().expect("one bind")))
            }
        }
        let receiver = receiver(ImportedPort::Valid(6001));
        let mut network = OneSocket {
            socket: Some(Script { stop: receiver.stop.clone(), shared: receiver.shared.clone(), receives: 0 }),
            opens: 0,
        };
        receiver.run_with(&mut network);
        assert_eq!(network.opens, 1);
        let shared = receiver.shared.lock().unwrap();
        assert!(shared.connected);
        assert_eq!(shared.values.longitudinal_acceleration, Some(7.0));
        assert_eq!(shared.values.max_rpm, Some(10000.0));
    }

    /// One datagram is one publication: whenever a reader holds the shared state, the values of a motion packet
    /// are all from the same packet (the receive thread assigns them under the lock).
    #[test]
    fn a_reader_never_sees_part_of_a_motion_packet() {
        let receiver = Arc::new(receiver(ImportedPort::Valid(6001)));
        let writer = {
            let receiver = receiver.clone();
            thread::spawn(move || {
                for round in 0..20_000u32 {
                    let k = (round % 7) as f32;
                    receiver.on_receive(Ok(motion_packet([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, k, k, k])));
                }
            })
        };
        while !writer.is_finished() {
            let shared = receiver.shared.lock().unwrap();
            let values = &shared.values;
            assert_eq!(values.lateral_acceleration, values.longitudinal_acceleration);
            assert_eq!(values.vertical_acceleration, values.longitudinal_acceleration);
            assert_eq!(values.longitudinal_acceleration.is_some(), shared.connected);
        }
        writer.join().unwrap();
    }

    #[test]
    fn native_motion_forwarding_precedes_publication_and_bad_packets_keep_values() {
        use crate::udp_racing::forward::PortForwarding;
        use crate::udp_racing::parameters::Parameters;
        let mut receiver = receiver(ImportedPort::Valid(6001));
        let reads = Arc::new(Mutex::new(0));
        let shared = receiver.shared.clone();
        let counted = reads.clone();
        let forwarding = Arc::new(PortForwarding::default());
        forwarding.set_reader(Arc::new(move |key| {
            assert_eq!(key, "il2");
            let before = shared.try_lock().expect("forwarding occurs outside the channel lock");
            assert!(!before.connected);
            assert_eq!(before.values.longitudinal_acceleration, None);
            *counted.lock().unwrap() += 1;
            Some(Vec::new())
        }));
        let parameters = Arc::new(Parameters::default());
        parameters.set_listener_running("il2");
        receiver.forwarder = Arc::new(Forwarder::with(parameters, forwarding));
        receiver.on_receive(Ok(motion_packet([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 3.0, 0.0, 0.0])));
        assert_eq!(*reads.lock().unwrap(), 2, "udp_forward reads the section for its test and for its loop");
        receiver.on_receive(Ok(vec![0]));
        assert_eq!(*reads.lock().unwrap(), 2, "only 44-byte motion packets are forwarded");
        let shared = receiver.shared.lock().unwrap();
        assert!(shared.connected, "receiving an invalid telemetry packet still marks the socket connected");
        assert_eq!(shared.values.longitudinal_acceleration, Some(3.0));
        assert_eq!(shared.values.max_rpm, Some(10000.0));
    }

    #[test]
    fn motion_publication_exposes_only_the_completed_channel_prefix() {
        let shared = Mutex::new(Il2Values::default());
        let order = [
            "max_rpm",
            "lateral_acceleration",
            "vertical_acceleration",
            "longitudinal_acceleration",
            "yaw",
            "pitch",
            "yaw_rate",
            "pitch_rate",
            "roll_rate",
            "roll",
        ];
        let packet = motion_packet([0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 7.0, 8.0, 9.0]);
        let mut step = 0;
        assert!(process_with(&packet, |change| {
            change.apply(&mut shared.lock().unwrap());
            let current = shared.try_lock().expect("publication lock released before a getter");
            for (index, name) in order.iter().enumerate() {
                let value = current.native_channel(name);
                if index <= step {
                    assert_ne!(value, LegacyValue::Int(0), "published {name}");
                } else {
                    assert_eq!(value, LegacyValue::Int(0), "unpublished {name}");
                }
            }
            step += 1;
        }));
        assert_eq!(step, order.len());
    }

    #[test]
    fn telemetry_publication_retains_prior_rpm_until_speed_assignment_completes() {
        let mut values =
            Il2Values { speed: Some(1.0), rpm: Some(2.0), aircraft: Some("Old".into()), ..Il2Values::default() };
        let packet = telemetry_packet(&[(INDICATOR_SPEED, &[33.0]), (INDICATOR_RPM, &[4400.0])], &[name_event("New")]);
        let mut seen = Vec::new();
        assert!(process_with(&packet, |change| {
            change.apply(&mut values);
            seen.push((values.speed, values.rpm, values.aircraft.clone()));
        }));
        assert_eq!(
            seen,
            [
                (Some(1.0), Some(2.0), Some("Old".into())),
                (Some(33.0), Some(2.0), Some("Old".into())),
                (Some(33.0), Some(4400.0), Some("Old".into())),
                (Some(33.0), Some(4400.0), Some("New".into())),
            ]
        );
        let mut changes = 0;
        assert!(!process_with(&[0], |change| {
            change.apply(&mut values);
            changes += 1;
        }));
        assert_eq!(changes, 1, "max_rpm precedes a failed telemetry parse");
        assert_eq!(values.speed, Some(33.0));
    }

    fn motion_packet(values: [f32; 9]) -> Vec<u8> {
        let mut bytes = vec![0xAA; MOTION_HEADER_LEN];
        for v in values {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes
    }

    /// A telemetry-device packet: header, indicators `(id, values)`, events `(id, payload)`.
    fn telemetry_packet(indicators: &[(u16, &[f32])], events: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = vec![0u8; TELEMETRY_HEADER_LEN];
        bytes.push(indicators.len() as u8);
        for (id, values) in indicators {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.push(values.len() as u8);
            for v in *values {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
        bytes.push(events.len() as u8);
        for (id, payload) in events {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.push(payload.len() as u8);
            bytes.extend_from_slice(payload);
        }
        bytes
    }

    fn name_event(name: &str) -> (u16, Vec<u8>) {
        let mut payload = vec![name.len() as u8];
        payload.extend_from_slice(name.as_bytes());
        (EVENT_SET_FOCUS, payload)
    }

    fn view(values: &Il2Values) -> Telemetry {
        let mut t = Telemetry::off();
        clear_listener_channels(&mut t);
        values.write(&mut t);
        t
    }

    #[test]
    fn native_getters_read_current_shared_values_and_keep_their_types() {
        use LegacyValue::{Float, Int, Str};

        // A receiver-free source: update the shared state directly instead of opening a socket.
        let source = Il2Source {
            shared: Arc::new(Mutex::new(Shared::default())),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            rates: AngularRates::default(),
        };
        assert_eq!(source.native_channel("max_rpm"), Some(Int(0)));
        assert_eq!(source.native_channel("roll_rate"), Some(Int(0)));
        assert!(!source.shared.lock().unwrap().values.process(&[]));
        assert_eq!(source.native_channel("max_rpm"), Some(Int(10000)));
        assert_eq!(source.native_channel("rpm"), Some(Int(0)));
        source.shared.lock().unwrap().values.process(&motion_packet([0.0; 9]));
        assert_eq!(source.native_channel("roll_rate"), Some(Float(-0.0)));
        source
            .shared
            .lock()
            .unwrap()
            .values
            .process(&telemetry_packet(&[(INDICATOR_RPM, &[0.0]), (INDICATOR_SPEED, &[88.5])], &[name_event("Yak-1")]));
        assert_eq!(source.native_channel("rpm"), Some(Float(0.0)));
        assert_eq!(source.native_channel("speed"), Some(Float(88.5)));
        assert_eq!(source.native_channel("car_name"), Some(Str("Yak-1".into())));
        assert_eq!(source.native_channel("gear"), Some(Int(0)));
    }

    #[test]
    fn process_motion_packet() {
        let mut v = Il2Values::default();
        assert!(v.process(&motion_packet([0.5, 0.1, 2.0, 0.3, 0.2, 0.4, 1.5, -0.5, 0.25])));
        let t = view(&v);
        let deg = |x: f32| rad_to_deg(f64::from(x));
        assert_eq!(t.yaw, deg(0.5));
        assert_eq!(t.pitch, -deg(0.1));
        assert_eq!(t.roll, 180.0 - deg(2.0), "roll beyond 90 degrees is folded");
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.vertical_acceleration), (1.5, -0.5, 0.25));
        assert_eq!(t.max_rpm, 10000.0);
        let rates = v.rates();
        assert_eq!((rates.roll, rates.pitch, rates.yaw), (-deg(0.3), -deg(0.2), deg(0.4)));
    }

    #[test]
    fn parse_telemetrydevice_last_value_wins_and_names_the_aircraft() {
        let packet = telemetry_packet(
            &[(INDICATOR_RPM, &[2100.0, 2400.0]), (3, &[9.0]), (INDICATOR_SPEED, &[88.5])],
            &[(7, vec![1, 2, 3]), name_event("Bf 109 G-6")],
        );
        let mut v = Il2Values::default();
        assert!(v.process(&packet));
        let t = view(&v);
        assert_eq!((t.rpm, t.speed, t.max_rpm), (2400.0, 88.5, 10000.0));
        assert_eq!(t.vehicle, "Bf 109 G-6");
    }

    #[test]
    fn parse_telemetrydevice_requires_speed_and_rpm() {
        let mut v = Il2Values::default();
        assert!(!v.process(&telemetry_packet(&[(INDICATOR_RPM, &[2100.0])], &[])));
        assert_eq!(v.max_rpm, Some(10000.0), "max_rpm is set before the packet is parsed");
        assert_eq!(v.rpm, None);
    }

    #[test]
    fn parse_telemetrydevice_unknown_name_keeps_previous_aircraft() {
        let mut v = Il2Values::default();
        v.process(&telemetry_packet(&[(0, &[1.0]), (6, &[2.0])], &[name_event("Yak-1")]));
        v.process(&telemetry_packet(&[(0, &[1.0]), (6, &[2.0])], &[name_event("Unknown")]));
        v.process(&telemetry_packet(&[(0, &[1.0]), (6, &[2.0])], &[]));
        assert_eq!(v.aircraft.as_deref(), Some("Yak-1"));
    }

    #[test]
    fn parse_telemetrydevice_truncated_name_reads_what_is_left() {
        let mut packet = telemetry_packet(&[(0, &[1.0]), (6, &[2.0])], &[]);
        *packet.last_mut().unwrap() = 1;
        packet.extend_from_slice(&EVENT_SET_FOCUS.to_le_bytes());
        packet.extend_from_slice(&[0, 10]);
        packet.extend_from_slice(b"La-5");
        assert_eq!(parse_telemetry_device(&packet).unwrap().aircraft, "La-5");
    }

    #[test]
    fn parse_telemetrydevice_rejects_truncated_and_non_ascii() {
        assert_eq!(parse_telemetry_device(&[0; 10]), None, "no indicator count");
        let mut packet = telemetry_packet(&[(0, &[1.0]), (6, &[2.0])], &[]);
        packet.pop();
        assert_eq!(parse_telemetry_device(&packet), None, "no event count");
        let mut packet = telemetry_packet(&[(0, &[1.0]), (6, &[2.0])], &[]);
        *packet.last_mut().unwrap() = 1;
        packet.extend_from_slice(&[0, 0, 0, 2, 0xC3, 0xA9]);
        assert_eq!(parse_telemetry_device(&packet), None, "decode('ascii') fails");
    }

    #[test]
    fn parse_telemetrydevice_negative_indicator_count_reads_no_indicators() {
        let mut packet = telemetry_packet(&[], &[]);
        packet[TELEMETRY_HEADER_LEN] = 0xFF;
        assert_eq!(parse_telemetry_device(&packet), None, "no speed/rpm");
    }

    #[test]
    fn il2_main_loop_receives_in_the_background() {
        let port = free_port();
        let mut source = Il2Source::new(UdpConfig::port(port));
        assert!(!source.is_running().unwrap());
        let _sender = Sender::start(port, motion_packet([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 3.0, 0.0, 0.0]));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !source.is_running().unwrap() {
            assert!(std::time::Instant::now() < deadline, "no datagram received");
            thread::sleep(Duration::from_millis(5));
        }
        let mut t = Telemetry::off();
        assert!(source.poll(&mut t).unwrap());
        assert_eq!(t.longitudinal_acceleration, 3.0);
        assert_eq!(t.gear, 0.0);
    }
}

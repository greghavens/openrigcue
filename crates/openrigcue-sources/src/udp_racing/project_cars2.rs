//! Project CARS 2 / Project CARS 3 on console, "Project CARS 2" UDP protocol (listener `projectcars2udp`).
//!
//! Datagrams follow the game's UDP protocol: a 12-byte header with the packet type in byte 10.

use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::forward::Forwarder;
use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::zero_listener_channels;
use super::support::net::{Datagrams, Endpoint, Network, SystemNetwork};
use super::support::pymath::rad_to_deg;
use super::support::wire::{Fields, Short};
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 5606;
const RECEIVE_SIZE: usize = 2400;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
const RUMBLE_TERRAIN: [u8; 4] = [10, 25, 40, 41];

/// Bytes each packet type needs (header included); a shorter datagram is a decode failure.
fn required_len(packet_type: u8) -> usize {
    match packet_type {
        0 => 378, // telemetry
        1 => 306, // race data
        3 => 784, // time stats
        4 => 22,  // game state
        7 => 801, // timings
        _ => 12,  // header only
    }
}

/// The values the receive step copies out of a telemetry packet.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Motion {
    max_rpm: f64,
    rpm: f64,
    gear_num_gears: u8,
    speed: f64,
    steering: f64,
    orientation: [f64; 3],
    local_velocity: [f64; 3],
    local_acceleration: [f64; 3],
    terrain: [u8; 4],
}

/// A decoded datagram.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Packet {
    Telemetry(Motion),
    RaceData,
    Other(u8),
}

impl Packet {
    /// Decodes a datagram; `Err` for a datagram shorter than its type needs.
    fn read(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        let kind = f.u8(10)?;
        f.bytes(0, required_len(kind))?;
        Ok(match kind {
            0 => {
                let v3 = |o: usize| -> Result<[f64; 3], Short> { Ok([f.f32(o)?, f.f32(o + 4)?, f.f32(o + 8)?]) };
                Packet::Telemetry(Motion {
                    max_rpm: f64::from(f.u16(42)?),
                    rpm: f64::from(f.u16(40)?),
                    gear_num_gears: f.u8(45)?,
                    speed: f.f32(36)?,
                    steering: f64::from(f.i8(15)?),
                    orientation: v3(52)?,
                    local_velocity: v3(64)?,
                    local_acceleration: v3(100)?,
                    terrain: [f.u8(140)?, f.u8(141)?, f.u8(142)?, f.u8(143)?],
                })
            }
            1 => Packet::RaceData,
            other => Packet::Other(other),
        })
    }
}

/// The gear: the last hex digit of `gearNumGears`; `f` is reverse
/// (-1) and the digits `a`–`e` are not decimal and give 0.
fn convert_gear(gear_num_gears: u8) -> f64 {
    match gear_num_gears & 0x0F {
        0x0F => -1.0,
        g @ 0..=9 => f64::from(g),
        _ => 0.0,
    }
}

/// The Project CARS 2 UDP listener.
pub struct ProjectCars2Source {
    settings: ListenerSettings,
    imported_port: Option<ImportedPort>,
    net: Box<dyn Network>,
    forwarder: Forwarder,
    socket: Option<Box<dyn Datagrams>>,
    motion: Option<Motion>,
}

impl ProjectCars2Source {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    /// Cache the configured port as an integer without applying the socket range yet.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        let mut source = Self::with_network(ListenerSettings::with_port(0), net);
        source.imported_port = Some(port);
        source
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let forwarder = Forwarder::default();
        ProjectCars2Source { settings, imported_port: None, net, forwarder, socket: None, motion: None }
    }

    /// The liveness test while not connected: the first datagram must be a telemetry (0), race data (1)
    /// or type-3 packet.
    fn connect(&mut self) -> bool {
        self.connect_with(|message| tracing::debug!("{message}"))
    }

    fn connect_with(&mut self, mut report: impl FnMut(String)) -> bool {
        let endpoint = Endpoint::any(self.settings.port, RECEIVE_TIMEOUT);
        let opened = match self.imported_port {
            Some(port) => self.net.bind_imported(&endpoint, port),
            None => self.net.bind(&endpoint),
        };
        let mut socket = match opened {
            Ok(s) => s,
            Err(error) => {
                // Setup failures are silent here. Keep the compatibility constructor's logging.
                if self.imported_port.is_none() {
                    report(format!("pcars2 udp bind error {error}"));
                }
                return false;
            }
        };
        let Ok((data, _)) = socket.receive(RECEIVE_SIZE) else { return false };
        self.forwarder.forward(&data);
        if matches!(Packet::read(&data), Ok(Packet::Telemetry(_) | Packet::RaceData | Packet::Other(3))) {
            self.socket = Some(socket);
            true
        } else {
            false
        }
    }

    /// The receive step. A race-data packet fails (the car name is looked up,
    /// which race data does not have) and therefore disconnects.
    fn receive(&mut self) -> bool {
        let Some(socket) = self.socket.as_mut() else { return false };
        let Ok((data, _)) = socket.receive(RECEIVE_SIZE) else {
            self.socket = None;
            return false;
        };
        self.forwarder.forward(&data);
        match Packet::read(&data) {
            Ok(Packet::Telemetry(m)) => {
                self.motion = Some(m);
                true
            }
            Ok(Packet::Other(_)) => true,
            Ok(Packet::RaceData) | Err(_) => {
                self.socket = None;
                false
            }
        }
    }

    /// Writes the channels: the copied values, 0 for everything else (`car_name` is never set).
    fn write_channels(&self, t: &mut Telemetry) {
        zero_listener_channels(t);
        let Some(m) = self.motion else { return };
        t.max_rpm = m.max_rpm;
        t.rpm = m.rpm;
        t.gear = convert_gear(m.gear_num_gears);
        t.speed = m.speed;
        t.steering_wheel_angle = m.steering;
        t.lateral_acceleration = m.local_acceleration[0] / 10.0;
        t.vertical_acceleration = m.local_acceleration[1] / 1000.0;
        t.suspension_travel_lf = m.local_acceleration[1] / 1000.0;
        t.suspension_travel_lr = m.local_acceleration[1] / 1000.0;
        t.suspension_travel_rf = m.local_acceleration[1] / 1000.0;
        t.suspension_travel_rr = m.local_acceleration[1] / 1000.0;
        t.longitudinal_acceleration = -m.local_acceleration[2];
        t.vertical_velocity = m.local_velocity[1];
        t.lateral_velocity = -m.local_velocity[0];
        t.longitudinal_velocity = m.local_velocity[2];
        t.roll = rad_to_deg(m.orientation[2]);
        t.pitch = rad_to_deg(m.orientation[0]);
        t.yaw = rad_to_deg(m.orientation[1]);
        let rumble = |terrain: u8| if RUMBLE_TERRAIN.contains(&terrain) { 40.0 } else { 0.0 };
        t.rumble_pitch_lf = rumble(m.terrain[0]);
        t.rumble_pitch_rf = rumble(m.terrain[1]);
        t.rumble_pitch_lr = rumble(m.terrain[2]);
        t.rumble_pitch_rr = rumble(m.terrain[3]);
    }
}

impl Source for ProjectCars2Source {
    fn id(&self) -> &'static str {
        "projectcars2udp"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int};
        let Some(m) = self.motion else { return Some(Int(0)) };
        Some(match name {
            "max_rpm" => Int(m.max_rpm as i64),
            "rpm" => Int(m.rpm as i64),
            "gear" => Int(convert_gear(m.gear_num_gears) as i64),
            "steering_wheel_angle" => Int(m.steering as i64),
            "speed" => Float(m.speed),
            "lateral_acceleration" => Float(m.local_acceleration[0] / 10.0),
            "vertical_acceleration"
            | "suspension_travel_lf"
            | "suspension_travel_lr"
            | "suspension_travel_rf"
            | "suspension_travel_rr" => Float(m.local_acceleration[1] / 1000.0),
            "longitudinal_acceleration" => Float(-m.local_acceleration[2]),
            "vertical_velocity" => Float(m.local_velocity[1]),
            "lateral_velocity" => Float(-m.local_velocity[0]),
            "longitudinal_velocity" => Float(m.local_velocity[2]),
            "roll" => Float(rad_to_deg(m.orientation[2])),
            "pitch" => Float(rad_to_deg(m.orientation[0])),
            "yaw" => Float(rad_to_deg(m.orientation[1])),
            "rumble_pitch_lf" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[0]) { 40 } else { 0 }),
            "rumble_pitch_rf" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[1]) { 40 } else { 0 }),
            "rumble_pitch_lr" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[2]) { 40 } else { 0 }),
            "rumble_pitch_rr" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[3]) { 40 } else { 0 }),
            _ => Int(0),
        })
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.socket.is_some() || self.connect())
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry);
        Ok(fresh)
    }

    fn stop(&mut self) {
        self.socket = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_setup_failures_use_the_silent_catches_without_losing_motion() {
        use crate::udp_racing::socket::socket_creation_error;
        struct Fails(bool);
        impl Network for Fails {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn Datagrams>> {
                let error = std::io::Error::other("setup failed");
                Err(if self.0 { socket_creation_error(error) } else { error })
            }
        }
        for creation in [true, false] {
            let mut source =
                ProjectCars2Source::with_imported_network(ImportedPort::Valid(DEFAULT_PORT), Box::new(Fails(creation)));
            let Packet::Telemetry(motion) = Packet::read(&telemetry_packet(0x63)).unwrap() else { panic!("telemetry") };
            source.motion = Some(motion);
            let mut reports = Vec::new();
            assert!(!source.connect_with(|line| reports.push(line)));
            assert!(reports.is_empty());
            assert_eq!(source.motion, Some(motion));
        }
    }

    #[test]
    fn native_imported_port_retries_setup_and_keeps_prior_telemetry() {
        use crate::udp_racing::socket::socket_creation_error;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged {
            attempts: Arc<Mutex<Vec<ImportedPort>>>,
            ready: FakeNetwork,
        }
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native source must bind its cached integer")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(endpoint.timeout, Some(RECEIVE_TIMEOUT));
                assert!(!endpoint.broadcast && !endpoint.reuse_address);
                let step = {
                    let mut attempts = self.attempts.lock().unwrap();
                    attempts.push(port);
                    attempts.len()
                };
                match step {
                    1 => Err(socket_creation_error(io::Error::other("create failed"))),
                    2 => Err(io::Error::other("bind failed")),
                    3 => Err(io::Error::other("timeout failed")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port.for_bind()?);
                        self.ready.bind(&endpoint)
                    }
                }
            }
        }
        let ready = FakeNetwork::default();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        ready.push(6001, &telemetry_packet(0x63));
        ready.push(6001, &telemetry_packet(0x63));
        let mut source = ProjectCars2Source::with_imported_network(
            ImportedPort::Valid(6001),
            Box::new(Staged { attempts: attempts.clone(), ready }),
        );
        let Packet::Telemetry(motion) = Packet::read(&telemetry_packet(0x63)).unwrap() else { panic!("telemetry") };
        source.motion = Some(motion);
        let mut reports = Vec::new();
        for _ in 0..3 {
            assert!(!source.connect_with(|line| reports.push(line)));
            assert_eq!(source.motion, Some(motion));
            assert!(source.socket.is_none());
        }
        assert!(reports.is_empty());
        assert!(source.connect_with(|line| reports.push(line)));
        assert!(source.receive());
        assert_eq!(*attempts.lock().unwrap(), [ImportedPort::Valid(6001); 4]);
    }

    #[test]
    fn native_out_of_range_bind_returns_false_silently_and_keeps_prior_telemetry() {
        struct OutOfRange;
        impl Network for OutOfRange {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn Datagrams>> {
                panic!("range validation must not invent a valid port")
            }
        }
        let mut source = ProjectCars2Source::with_imported_network(ImportedPort::OutOfRange, Box::new(OutOfRange));
        let Packet::Telemetry(motion) = Packet::read(&telemetry_packet(0x63)).unwrap() else { panic!("telemetry") };
        source.motion = Some(motion);
        let mut reports = Vec::new();
        assert!(!source.connect_with(|line| reports.push(line)));
        assert!(reports.is_empty());
        assert_eq!(source.motion, Some(motion));
    }
    use crate::udp_racing::support::net::fake::FakeNetwork;

    #[test]
    fn native_output_kinds() {
        let mut s =
            ProjectCars2Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Int(0)));
        let Packet::Telemetry(motion) = Packet::read(&telemetry_packet(0x63)).unwrap() else { panic!("telemetry") };
        s.motion = Some(motion);
        for (name, value) in [
            ("rpm", LegacyValue::Int(7000)),
            ("max_rpm", LegacyValue::Int(8500)),
            ("gear", LegacyValue::Int(3)),
            ("steering_wheel_angle", LegacyValue::Int(100)),
            ("speed", LegacyValue::Float(50.0)),
            ("lateral_velocity", LegacyValue::Float(-0.0)),
            ("rumble_pitch_rf", LegacyValue::Int(40)),
            ("rumble_pitch_lf", LegacyValue::Int(0)),
            ("car_name", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(value), "{name}");
        }
        s.motion.as_mut().unwrap().gear_num_gears = 0xff;
        assert_eq!(s.native_channel("gear"), Some(LegacyValue::Int(-1)));
        s.motion.as_mut().unwrap().gear_num_gears = 0xaa;
        assert_eq!(s.native_channel("gear"), Some(LegacyValue::Int(0)));
    }

    /// A type-0 (telemetry) packet.
    fn telemetry_packet(gear_num_gears: u8) -> Vec<u8> {
        let mut p = vec![0u8; 559];
        p[15] = 100;
        p[36..40].copy_from_slice(&50.0f32.to_le_bytes());
        p[40..42].copy_from_slice(&7000u16.to_le_bytes());
        p[42..44].copy_from_slice(&8500u16.to_le_bytes());
        p[45] = gear_num_gears;
        p[60..64].copy_from_slice(&1.0f32.to_le_bytes());
        p[68..72].copy_from_slice(&0.75f32.to_le_bytes());
        p[100..104].copy_from_slice(&(-3.0f32).to_le_bytes());
        p[140..144].copy_from_slice(&[0, 41, 0, 25]);
        p
    }

    fn typed(kind: u8, len: usize) -> Vec<u8> {
        let mut p = vec![0u8; len];
        p[10] = kind;
        p
    }

    fn connected(first: &[u8]) -> (ProjectCars2Source, FakeNetwork) {
        let net = FakeNetwork::default();
        let mut s = ProjectCars2Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, first);
        assert!(s.is_running().unwrap());
        (s, net)
    }

    #[test]
    fn convert_gear_reads_the_last_hex_digit() {
        // Gear: a last hex digit 'f' -> -1; 'a'..'e' are not decimal -> 0
        assert_eq!(convert_gear(0x64), 4.0);
        assert_eq!(convert_gear(0x6F), -1.0);
        assert_eq!(convert_gear(0x6A), 0.0);
        assert_eq!(convert_gear(0x00), 0.0);
    }

    #[test]
    fn check_status_accepts_types_0_1_and_3() {
        // Liveness: packet type 0, 1 or 3
        connected(&telemetry_packet(0x63));
        connected(&typed(1, 306));
        connected(&typed(3, 784));
        let net = FakeNetwork::default();
        let mut s = ProjectCars2Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &typed(4, 22));
        assert!(!s.is_running().unwrap());
        net.push(DEFAULT_PORT, &typed(1, 100));
        assert!(!s.is_running().unwrap(), "short RaceData raises EndOfFileError");
    }

    #[test]
    fn telemetry_mapping() {
        // Receive step, packet type 0
        let (mut s, net) = connected(&telemetry_packet(0x63));
        net.push(DEFAULT_PORT, &telemetry_packet(0x63));
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed, t.steering_wheel_angle), (7000.0, 8500.0, 3.0, 50.0, 100.0));
        assert_eq!(t.lateral_velocity, -0.0);
        assert_eq!(t.vertical_velocity, 0.75);
        assert_eq!(t.lateral_acceleration, -0.3);
        assert!((t.roll - 57.29577951308232).abs() < 1e-12);
        assert_eq!(
            [t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr],
            [0.0, 40.0, 0.0, 40.0]
        );
    }

    #[test]
    fn race_data_disconnects() {
        // Receive step: race data has no car name, the lookup fails -> not connected
        let (mut s, net) = connected(&telemetry_packet(0x63));
        net.push(DEFAULT_PORT, &typed(1, 306));
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        net.push(DEFAULT_PORT, &telemetry_packet(0x63));
        assert!(s.is_running().unwrap());
        assert_eq!(net.bound().len(), 2, "rebinds after the race-data packet");
        net.push(DEFAULT_PORT, &typed(7, 801));
        assert!(s.poll(&mut t).unwrap(), "other classes are ignored");
    }
}

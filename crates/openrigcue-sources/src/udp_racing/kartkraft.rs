//! KartKraft FlatBuffers telemetry, file identifier `KKFB` (listener `kartkraft`).

use std::collections::BTreeSet;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::parameters::GameErrors;
pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::zero_listener_channels;
use super::support::flatbuffers::Table;
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::receiver::{ReceiveFailure, Receiver};
use super::support::wire::Short;
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 5000;
const RECEIVE_SIZE: usize = 1024;
const RECEIVE_TIMEOUT: Duration = Duration::from_millis(200);
const IDENTIFIER: &[u8; 4] = b"KKFB";
/// Index of the 'Tyre' surface; the list has seven names.
const SURFACE_TYRE: u8 = 6;
const SURFACE_COUNT: u8 = 7;

/// Corners in wheel order: lr, lf, rf, rr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Corner {
    LeftRear,
    LeftFront,
    RightFront,
    RightRear,
}
const WHEELS: [Corner; 4] = [Corner::LeftRear, Corner::LeftFront, Corner::RightFront, Corner::RightRear];

/// The nine motion channels the receive step copies (or zeroes) together.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct MotionValues {
    /// Accessors return floats, while the receive step's failure path writes integer zeroes.
    native_float: bool,
    yaw: f64,
    roll: f64,
    pitch: f64,
    vertical_acceleration: f64,
    lateral_acceleration: f64,
    longitudinal_acceleration: f64,
    longitudinal_velocity: f64,
    lateral_velocity: f64,
    vertical_velocity: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct DashValues {
    /// RPM/speed/steer accessors return floats; the whole-block fallback writes integer zeroes.
    native_float: bool,
    rpm: f64,
    speed: f64,
    steer: f64,
    gear: f64,
}

/// The stored telemetry values: keys appear once written and keep their last value.
#[derive(Debug, Clone, Default, PartialEq)]
struct KartTelemetry {
    motion: Option<MotionValues>,
    rumble: [Option<f64>; 4],
    slip: [Option<f64>; 4],
    dash: Option<DashValues>,
    max_rpm: Option<f64>,
    max_rpm_float: bool,
}

fn corner_index(c: Corner) -> usize {
    c as usize
}

impl KartTelemetry {
    /// The motion block of the receive step: on any failure (including a fifth wheel or an unknown
    /// surface index) the nine motion channels are zeroed, keeping wheel values already written. `true` when
    /// the whole block was copied (the game errors are then cleared).
    fn apply_motion(&mut self, motion: Option<Table>) -> bool {
        let copied = motion.is_some_and(|m| self.copy_motion(&m).is_ok());
        if !copied {
            self.motion = Some(MotionValues::default());
        }
        copied
    }

    fn copy_motion(&mut self, m: &Table) -> Result<(), Short> {
        self.motion = Some(MotionValues {
            native_float: true,
            yaw: m.f32(8, 0.0)?,
            roll: m.f32(6, 0.0)?,
            pitch: m.f32(4, 0.0)?,
            vertical_acceleration: m.f32(14, 0.0)?,
            lateral_acceleration: -m.f32(12, 0.0)?,
            longitudinal_acceleration: m.f32(10, 0.0)?,
            longitudinal_velocity: m.f32(18, 0.0)?,
            lateral_velocity: m.f32(20, 0.0)?,
            vertical_velocity: m.f32(22, 0.0)?,
        });
        for i in 0..m.vector_len(30)? {
            let wheel = m.vector_table(30, i)?.ok_or(Short)?;
            let surface = wheel.u8(4)?;
            if surface >= SURFACE_COUNT {
                return Err(Short);
            }
            let corner = corner_index(*WHEELS.get(i).ok_or(Short)?);
            self.rumble[corner] = Some(if surface == SURFACE_TYRE { 40.0 } else { 0.0 });
            self.slip[corner] = Some(wheel.f32(6, 0.0)?);
        }
        Ok(())
    }

    /// The dashboard block: `Rpm`, `Speed`, `Steer`, `Gear`, all 0 without a dashboard.
    fn apply_dash(&mut self, dash: Option<Table>) {
        let read = |d: &Table| -> Result<DashValues, Short> {
            Ok(DashValues {
                native_float: true,
                rpm: d.f32(6, 0.0)?,
                speed: d.f32(4, 0.0)?,
                steer: d.f32(8, 0.0)?,
                gear: f64::from(d.i8(14)?),
            })
        };
        self.dash = Some(dash.and_then(|d| read(&d).ok()).unwrap_or_default());
    }

    /// The vehicle-config block: `RpmMax`, 0 without one.
    fn apply_vehicle_config(&mut self, config: Option<Table>) {
        let value = config.and_then(|c| c.f32(6, 0.0).ok());
        self.max_rpm_float = value.is_some();
        self.max_rpm = Some(value.unwrap_or(0.0));
    }

    fn native(&self, channel: &str) -> LegacyValue {
        use LegacyValue::{Float, Int};
        if let Some(m) = self.motion {
            let value = match channel {
                "yaw" => Some(m.yaw),
                "roll" => Some(m.roll),
                "pitch" => Some(m.pitch),
                "vertical_acceleration" => Some(m.vertical_acceleration),
                "lateral_acceleration" => Some(m.lateral_acceleration),
                "longitudinal_acceleration" => Some(m.longitudinal_acceleration),
                "longitudinal_velocity" => Some(m.longitudinal_velocity),
                "lateral_velocity" => Some(m.lateral_velocity),
                "vertical_velocity" => Some(m.vertical_velocity),
                _ => None,
            };
            if let Some(value) = value {
                return if m.native_float { Float(value) } else { Int(0) };
            }
        }
        if let Some(d) = self.dash {
            let value = match channel {
                "rpm" => Some(d.rpm),
                "speed" => Some(d.speed),
                "steering_wheel_angle" => Some(d.steer),
                "gear" => return Int(d.gear as i64),
                _ => None,
            };
            if let Some(value) = value {
                return if d.native_float { Float(value) } else { Int(0) };
            }
        }
        let float = |value: Option<f64>| value.map_or(Int(0), Float);
        let integer = |value: Option<f64>| value.map_or(Int(0), |v| Int(v as i64));
        match channel {
            "rumble_pitch_lr" => integer(self.rumble[corner_index(Corner::LeftRear)]),
            "rumble_pitch_lf" => integer(self.rumble[corner_index(Corner::LeftFront)]),
            "rumble_pitch_rf" => integer(self.rumble[corner_index(Corner::RightFront)]),
            "rumble_pitch_rr" => integer(self.rumble[corner_index(Corner::RightRear)]),
            "wheel_slip_lr" => float(self.slip[corner_index(Corner::LeftRear)]),
            "wheel_slip_lf" => float(self.slip[corner_index(Corner::LeftFront)]),
            "wheel_slip_rf" => float(self.slip[corner_index(Corner::RightFront)]),
            "wheel_slip_rr" => float(self.slip[corner_index(Corner::RightRear)]),
            "max_rpm" if self.max_rpm_float => float(self.max_rpm),
            _ => Int(0),
        }
    }

    /// A channel read: the stored value or 0.
    fn write(&self, t: &mut Telemetry) {
        zero_listener_channels(t);
        if let Some(m) = self.motion {
            t.yaw = m.yaw;
            t.roll = m.roll;
            t.pitch = m.pitch;
            t.vertical_acceleration = m.vertical_acceleration;
            t.lateral_acceleration = m.lateral_acceleration;
            t.longitudinal_acceleration = m.longitudinal_acceleration;
            t.longitudinal_velocity = m.longitudinal_velocity;
            t.lateral_velocity = m.lateral_velocity;
            t.vertical_velocity = m.vertical_velocity;
        }
        let value = |v: Option<f64>| v.unwrap_or(0.0);
        let [lr, lf, rf, rr] = self.rumble;
        (t.rumble_pitch_lr, t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_rr) =
            (value(lr), value(lf), value(rf), value(rr));
        let [lr, lf, rf, rr] = self.slip;
        (t.wheel_slip_lr, t.wheel_slip_lf, t.wheel_slip_rf, t.wheel_slip_rr) =
            (value(lr), value(lf), value(rf), value(rr));
        if let Some(d) = self.dash {
            (t.rpm, t.speed, t.steering_wheel_angle, t.gear) = (d.rpm, d.speed, d.steer, d.gear);
        }
        if let Some(max) = self.max_rpm {
            t.max_rpm = max;
        }
    }
}

/// The KartKraft listener.
pub struct KartKraftSource {
    receiver: Receiver,
    datagram: Vec<u8>,
    state: KartTelemetry,
    game_errors: GameErrors,
    native_socket: bool,
}

impl KartKraftSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    /// Cache `NETWORK_PORT.kartkraft` as an integer, leaving the range check to the bind.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self {
            receiver: Receiver::with_imported_port(net, Endpoint::loopback(0, RECEIVE_TIMEOUT), RECEIVE_SIZE, port),
            datagram: Vec::new(),
            state: KartTelemetry::default(),
            game_errors: GameErrors::default(),
            native_socket: true,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, RECEIVE_TIMEOUT);
        let receiver = Receiver::new(net, endpoint, RECEIVE_SIZE);
        KartKraftSource {
            receiver,
            datagram: Vec::new(),
            state: KartTelemetry::default(),
            game_errors: GameErrors::default(),
            native_socket: false,
        }
    }

    /// The error texts this listener contributes to the game errors.
    pub fn game_errors(&self) -> &BTreeSet<String> {
        self.game_errors.messages()
    }

    /// The receive step. A failed frame lookup (the root, motion, vehicle-config and dashboard tables)
    /// is not handled: a frame whose root or table offsets cannot be read fails
    /// before any value is written and ends the dispatcher thread ([`DispatcherEnds`]).
    fn decode(&mut self) -> Result<bool, SourceError> {
        if !Table::has_identifier(&self.datagram, IDENTIFIER) {
            return Ok(false);
        }
        let tables = Table::root(&self.datagram).and_then(|root| Ok((root.table(6)?, root.table(12)?, root.table(8)?)));
        let (motion, vehicle_config, dash) =
            tables.map_err(|_| DispatcherEnds::at_receive_data("kartkraft receiveData: frame tables out of range"))?;
        if self.state.apply_motion(motion) {
            self.game_errors.clear();
        }
        self.state.apply_dash(dash);
        self.state.apply_vehicle_config(vehicle_config);
        Ok(true)
    }
}

impl Source for KartKraftSource {
    fn id(&self) -> &'static str {
        "kartkraft"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        Some(self.state.native(name))
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        self.decode()
    }

    /// The liveness test: true for a non-empty datagram; a bind failure is recorded in the game errors.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_once() {
            Ok(data) => {
                self.datagram = data;
                !self.datagram.is_empty()
            }
            Err(ReceiveFailure::Bind(e)) => {
                if !self.native_socket || !socket_creation_failed(&e) {
                    self.game_errors.record(&e);
                }
                false
            }
            Err(ReceiveFailure::Receive(_)) => false,
        };
        Ok(running)
    }

    /// The receive step and the channel reads; nothing is written where the receive step fails.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.state.write(telemetry);
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_failure_does_not_publish_a_bind_error() {
        struct Fails;
        impl Network for Fails {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn super::super::support::net::Datagrams>> {
                Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("create failed")))
            }
        }
        let mut source = KartKraftSource::with_imported_network(ImportedPort::Valid(DEFAULT_PORT), Box::new(Fails));
        assert!(!source.is_running().unwrap());
        assert!(source.game_errors().is_empty(), "creation has a separate silent catch");
    }

    #[test]
    fn native_socket_retry_retains_data_and_clears_only_after_the_motion_block() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::{Arc, Mutex};
        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native bind")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                let step = {
                    let mut calls = self.0.lock().unwrap();
                    calls.push(port);
                    calls.len()
                };
                match step {
                    1 => Err(crate::udp_racing::socket::socket_creation_error(io::Error::other("create failed"))),
                    2 => Err(io::Error::other("bind failed")),
                    3 => Err(io::Error::other("timeout failed")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port.for_bind()?);
                        self.1.bind(&endpoint)
                    }
                }
            }
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let ready = FakeNetwork::default();
        ready.push(6001, &frame(vec![wheel(0, 1.0); 4]));
        let mut source =
            KartKraftSource::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), ready)));
        source.datagram = vec![9];
        for count in [0, 1, 2] {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.game_errors().len(), count);
            assert_eq!(source.datagram, [9]);
        }
        assert!(source.is_running().unwrap());
        assert_eq!(source.game_errors().len(), 2);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert!(source.game_errors().is_empty());
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 4]);
    }
    use crate::udp_racing::support::flatbuffers::build::{buffer, Value};
    use crate::udp_racing::support::net::fake::FakeNetwork;

    fn wheel(surface: u8, slip: f32) -> Vec<(u16, Value)> {
        vec![(4, Value::U8(surface)), (6, Value::F32(slip))]
    }

    fn frame(wheels: Vec<Vec<(u16, Value)>>) -> Vec<u8> {
        let motion = vec![
            (4, Value::F32(1.0)),
            (6, Value::F32(2.0)),
            (8, Value::F32(3.0)),
            (10, Value::F32(0.5)),
            (12, Value::F32(0.25)),
            (14, Value::F32(9.0)),
            (18, Value::F32(10.0)),
            (20, Value::F32(0.75)),
            (22, Value::F32(-0.5)),
            (30, Value::Tables(wheels)),
        ];
        let dash = vec![(4, Value::F32(20.0)), (6, Value::F32(12000.0)), (8, Value::F32(-0.3)), (14, Value::I8(-1))];
        buffer(
            IDENTIFIER,
            &[(6, Value::Table(motion)), (8, Value::Table(dash)), (12, Value::Table(vec![(6, Value::F32(15000.0))]))],
        )
    }

    fn polled(datagram: &[u8]) -> (bool, Telemetry) {
        let net = FakeNetwork::default();
        let mut s = KartKraftSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, datagram);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        let fresh = s.poll(&mut t).unwrap();
        (fresh, t)
    }

    #[test]
    fn native_getters_retain_stored_float_and_integer_kinds() {
        let mut s =
            KartKraftSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        s.datagram = frame(vec![wheel(6, 0.1)]);
        s.receive_native(&mut Telemetry::off()).unwrap();
        for (name, value) in [
            ("rpm", LegacyValue::Float(12000.0)),
            ("speed", LegacyValue::Float(20.0)),
            ("max_rpm", LegacyValue::Float(15000.0)),
            ("gear", LegacyValue::Int(-1)),
            ("yaw", LegacyValue::Float(3.0)),
            ("rumble_pitch_lr", LegacyValue::Int(40)),
            ("wheel_slip_lr", LegacyValue::Float(f64::from(0.1f32))),
            ("wheel_slip_lf", LegacyValue::Int(0)),
            ("car_name", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(value), "{name}");
        }
        s.datagram = b"wrong identifier".to_vec();
        assert!(!s.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Float(12000.0)));
    }

    #[test]
    fn native_absent_blocks_and_caught_motion_resets_are_integer_zeroes() {
        let mut s =
            KartKraftSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        assert_eq!(s.native_channel("yaw"), Some(LegacyValue::Int(0)));
        s.datagram = buffer(IDENTIFIER, &[]);
        assert!(s.receive_native(&mut Telemetry::off()).unwrap());
        for name in ["yaw", "speed", "rpm", "gear", "max_rpm", "wheel_slip_lr", "missing"] {
            assert_eq!(s.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
        }
        s.datagram = frame(vec![wheel(6, 0.1), wheel(0, 0.2), wheel(0, 0.3), wheel(0, 0.4), wheel(6, 0.5)]);
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.native_channel("yaw"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("lateral_acceleration"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("rumble_pitch_lr"), Some(LegacyValue::Int(40)));
        assert_eq!(s.native_channel("wheel_slip_lr"), Some(LegacyValue::Float(f64::from(0.1f32))));
        assert_eq!(s.native_channel("rpm"), Some(LegacyValue::Float(12000.0)));
    }

    #[test]
    fn native_empty_scalar_fields_keep_accessor_float_defaults() {
        let mut s =
            KartKraftSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        s.datagram = buffer(
            IDENTIFIER,
            &[
                (6, Value::Table(vec![(30, Value::Tables(vec![vec![]]))])),
                (8, Value::Table(vec![])),
                (12, Value::Table(vec![])),
            ],
        );
        s.receive_native(&mut Telemetry::off()).unwrap();
        for name in ["yaw", "roll", "pitch", "rpm", "speed", "max_rpm", "wheel_slip_lr"] {
            assert_eq!(s.native_channel(name), Some(LegacyValue::Float(0.0)), "{name}");
        }
        assert!(
            matches!(s.native_channel("lateral_acceleration"), Some(LegacyValue::Float(n)) if n.to_bits() == (-0.0_f64).to_bits())
        );
        assert_eq!(s.native_channel("rumble_pitch_lr"), Some(LegacyValue::Int(0)));
        assert_eq!(s.native_channel("gear"), Some(LegacyValue::Int(0)));
    }

    #[test]
    fn source_adapter_keeps_receive_failure_and_preceding_native_values() {
        use crate::derive::{FailurePoint, Listener, SourceListener};
        use crate::NativeRead;
        let net = FakeNetwork::default();
        let s = KartKraftSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let mut listener = SourceListener::new(s);
        net.push(DEFAULT_PORT, &frame(vec![]));
        assert!(listener.is_running());
        listener.receive();
        assert_eq!(listener.read_native("rpm"), NativeRead::Value(LegacyValue::Float(12000.0)));
        let mut malformed = vec![0xff; 8];
        malformed[4..].copy_from_slice(IDENTIFIER);
        net.push(DEFAULT_PORT, &malformed);
        assert!(listener.is_running());
        listener.receive();
        assert_eq!(listener.failure_point(), Some(FailurePoint::ReceiveData));
        assert_eq!(listener.read_native("rpm"), NativeRead::Value(LegacyValue::Float(12000.0)));
    }

    #[test]
    fn motion_dash_and_config() {
        // Receive step: motion, dash and vehicle-config blocks
        let (fresh, t) = polled(&frame(vec![wheel(6, 0.1), wheel(4, 0.2), wheel(1, 0.3), wheel(6, 0.4)]));
        assert!(fresh);
        assert_eq!((t.pitch, t.roll, t.yaw), (1.0, 2.0, 3.0));
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.vertical_acceleration), (0.5, -0.25, 9.0));
        assert_eq!((t.longitudinal_velocity, t.lateral_velocity, t.vertical_velocity), (10.0, 0.75, -0.5));
        // Wheel order lr, lf, rf, rr; only the 'Tyre' surface rumbles (not 'Kerb')
        assert_eq!(
            [t.rumble_pitch_lr, t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_rr],
            [40.0, 0.0, 0.0, 40.0]
        );
        assert_eq!(t.wheel_slip_lf, f64::from(0.2f32));
        assert_eq!(
            (t.rpm, t.speed, t.steering_wheel_angle, t.gear, t.max_rpm),
            (12000.0, 20.0, f64::from(-0.3f32), -1.0, 15000.0)
        );
    }

    #[test]
    fn a_fifth_wheel_zeroes_motion_but_keeps_wheels() {
        // Receive step: a fifth wheel is out of range -> motion channels 0
        let (_, t) = polled(&frame(vec![wheel(6, 0.1), wheel(0, 0.2), wheel(0, 0.3), wheel(0, 0.4), wheel(6, 0.5)]));
        assert_eq!((t.pitch, t.yaw), (0.0, 0.0));
        assert_eq!(t.rumble_pitch_lr, 40.0);
        assert_eq!(t.rpm, 12000.0);
    }

    #[test]
    fn bind_errors_are_recorded_until_a_complete_motion_block() {
        // Liveness: a bind failure adds its text to the game errors;
        // the receive step clears them after the wheel loop of the motion block
        let net = FakeNetwork::default();
        let mut s = KartKraftSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        assert!(!s.is_running().unwrap());
        assert!(s.game_errors().is_empty(), "receive timeout");
        net.refuse_bind(true);
        assert!(!s.is_running().unwrap());
        assert_eq!(s.game_errors().iter().collect::<Vec<_>>(), [crate::udp_racing::parameters::PORT_IN_USE]);
        net.refuse_bind(false);
        net.push(DEFAULT_PORT, &frame(vec![wheel(0, 0.0); 5]));
        assert!(s.is_running().unwrap());
        s.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(s.game_errors().len(), 1, "the fifth wheel raised before the clear");
        net.push(DEFAULT_PORT, &frame(vec![wheel(0, 0.0)]));
        assert!(s.is_running().unwrap());
        s.poll(&mut Telemetry::off()).unwrap();
        assert!(s.game_errors().is_empty());
    }

    #[test]
    fn malformed_frame_tables_raise_out_of_receive_data() {
        // Receive step: a failed root or motion-table lookup is not handled
        let net = FakeNetwork::default();
        let mut s = KartKraftSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &frame(vec![wheel(6, 0.1)]));
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        let mut bad = b"\xff\xff\x00\x00KKFB".to_vec();
        bad.resize(16, 0);
        net.push(DEFAULT_PORT, &bad);
        assert!(s.is_running().unwrap());
        let mut after = t.clone();
        after.rpm = -1.0;
        assert!(s.poll(&mut after).is_err_and(|e| crate::udp_racing::unhandled::ends_dispatcher(&e)));
        assert_eq!(after.rpm, -1.0, "nothing is written that tick");
    }

    #[test]
    fn wrong_identifier_is_ignored() {
        // Receive step: a buffer without the file identifier -> false
        let mut d = frame(vec![]);
        d[4] = b'X';
        let (fresh, t) = polled(&d);
        assert!(!fresh);
        assert_eq!(t.rpm, 0.0);
        assert_eq!(t.gear, 0.0, "the empty telemetry dict answers 0");
    }
}

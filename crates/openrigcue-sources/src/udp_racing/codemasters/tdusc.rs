//! Test Drive Unlimited Solar Crown (listener `tdusc`).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::kinematics::{lateral_speed, tdusc_orientation, vertical_speed, HeaveEstimator, HeaveRule};
use super::packet::{Layout, MotionPacket};
use super::{publish_speed_unreadable, publish_unmapped_as_zero, stationary, CarName, GuardRule, Stationary};
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::listener_output::python_int;
use crate::udp_racing::parameters::GameErrors;
use crate::udp_racing::socket::{
    socket_creation_failed, DatagramSocket, Endpoint, ImportedPort, ListenerSettings, UdpEndpoint,
};
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{NativeRead, Source, SourceError};

/// Receive timeout.
const TIMEOUT: Duration = Duration::from_millis(200);
/// Receive buffer size.
const RECEIVE_SIZE: usize = 512;
/// The game sends the 70-float WRC-style packet.
const PACKET_SIZE: usize = 280;
/// `max_rpm` when reading it fails.
const FALLBACK_MAX_RPM: f64 = 20000.0;

/// The TDU Solar Crown listener: synchronous, like DiRT.
pub struct TduscSource {
    socket: Box<dyn DatagramSocket>,
    native_socket: bool,
    socket_assigned: bool,
    forwarder: Arc<Forwarder>,
    connected: bool,
    packet: Option<MotionPacket>,
    heave: Mutex<HeaveEstimator>,
    /// This listener's share of the game errors.
    game_errors: GameErrors,
}

impl TduscSource {
    /// Listen on `127.0.0.1:<settings.port>`.
    pub fn new(settings: &ListenerSettings) -> Self {
        let socket = UdpEndpoint::new(Endpoint::loopback(settings.port, TIMEOUT));
        TduscSource::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    pub fn from_imported_port(port: ImportedPort) -> Self {
        let socket = UdpEndpoint::from_imported_port(std::net::Ipv4Addr::LOCALHOST, port, TIMEOUT);
        Self::with_native_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    fn with_native_socket(socket: Box<dyn DatagramSocket>, forwarder: Arc<Forwarder>) -> Self {
        Self { native_socket: true, ..Self::with_socket(socket, forwarder) }
    }

    /// Use the given socket and forwarder (tests).
    pub fn with_socket(socket: Box<dyn DatagramSocket>, forwarder: Arc<Forwarder>) -> Self {
        TduscSource {
            socket,
            native_socket: false,
            socket_assigned: false,
            forwarder,
            connected: false,
            packet: None,
            heave: Mutex::new(HeaveEstimator::new(HeaveRule::Raw)),
            game_errors: GameErrors::default(),
        }
    }

    /// The texts this listener added to the game errors (a failed bind or timeout setting in
    /// the liveness test), emptied when the receive step takes in a datagram.
    pub fn game_errors(&self) -> &std::collections::BTreeSet<String> {
        self.game_errors.messages()
    }

    fn disconnect(&mut self) {
        self.socket.close();
        self.connected = false;
    }

    /// The receive step: a 280-byte datagram is parsed as an extradata=3 packet from its first 256 bytes and
    /// keeps the game connected; any other length disconnects.
    fn receive(&mut self) -> bool {
        let datagram = match self.socket.receive(RECEIVE_SIZE) {
            Ok(datagram) => datagram,
            Err(_) => {
                self.disconnect();
                return false;
            }
        };
        self.forwarder.forward(&datagram);
        // The game errors are cleared after both branches of the length test.
        self.game_errors.clear();
        if datagram.len() != PACKET_SIZE {
            self.disconnect();
            return false;
        }
        self.packet = MotionPacket::decode_prefix(&datagram, Layout::Extradata3);
        self.connected = true;
        true
    }

    /// Writes every registry channel the listener answers.
    fn publish(&mut self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        // No packet yet: nothing fails, every channel reads 0.
        let Some(p) = self.packet else {
            publish_unmapped_as_zero(t, CarName::Blank);
            return Ok(());
        };
        match stationary(&p, GuardRule::RpmSpeed) {
            Stationary::Unreadable => return publish_speed_unreadable(t, 0.0, CarName::Raised),
            Stationary::Yes => {
                publish_unmapped_as_zero(t, CarName::Blank);
                return Ok(());
            }
            Stationary::No => {}
        }
        let lateral_velocity = -lateral_speed(&p.frame);
        let Some(speed) = python_int(p.speed) else {
            return publish_speed_unreadable(t, lateral_velocity, CarName::Blank);
        };
        // A failed integer conversion leaves the channel as it was (no registry default).
        let rpm = python_int(p.rpm).unwrap_or(t.rpm);
        let gear = python_int(p.gear).unwrap_or(t.gear);
        publish_unmapped_as_zero(t, CarName::Blank);
        t.speed = speed;
        t.rpm = rpm;
        t.max_rpm = p.session.and_then(|s| python_int(s.max_rpm)).unwrap_or(FALLBACK_MAX_RPM);
        t.gear = gear;
        t.suspension_travel_lf = p.suspension_position.front_left / 10000.0;
        t.suspension_travel_lr = p.suspension_position.rear_left / 10000.0;
        t.suspension_travel_rf = p.suspension_position.front_right / 10000.0;
        t.suspension_travel_rr = p.suspension_position.rear_right / 10000.0;
        t.suspension_velocity_lr = p.suspension_velocity.rear_left;
        t.suspension_velocity_rr = p.suspension_velocity.rear_right;
        t.suspension_velocity_lf = p.suspension_velocity.front_left;
        t.suspension_velocity_rf = p.suspension_velocity.front_right;
        t.steering_wheel_angle = p.steer;
        t.longitudinal_acceleration = -p.gforce_lon;
        t.lateral_acceleration = p.gforce_lat;
        (t.pitch, t.yaw, t.roll) = tdusc_orientation(&p.frame);
        t.lateral_velocity = lateral_velocity;
        t.vertical_acceleration =
            -self.heave.get_mut().unwrap_or_else(PoisonError::into_inner).next(p.time, vertical_speed(&p.frame));
        Ok(())
    }

    /// The liveness test (the same state machine as the DiRT listener's).
    fn check_status(&mut self) -> Result<bool, SourceError> {
        if self.connected {
            return Ok(true);
        }
        if let Err(error) = self.socket.reopen() {
            if self.native_socket && socket_creation_failed(&error) {
                if !self.socket_assigned {
                    return Err(
                        DispatcherEnds::new("AttributeError: 'NoneType' object has no attribute 'close'").into()
                    );
                }
                self.disconnect();
                return Ok(false);
            }
            self.socket_assigned = true;
            self.game_errors.record(&error);
            self.disconnect();
            return Ok(false);
        }
        self.socket_assigned = true;
        Ok(match self.socket.receive(RECEIVE_SIZE) {
            Ok(datagram) => {
                self.forwarder.forward(&datagram);
                self.connected = true;
                true
            }
            Err(_) => {
                self.disconnect();
                false
            }
        })
    }
}

impl Source for TduscSource {
    fn id(&self) -> &'static str {
        "tdusc"
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        self.check_status()
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }
    fn read_native(&self, name: &str) -> NativeRead {
        super::native::read(self.packet, &self.heave, super::native::Kind::Tdusc, name)
    }
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            _ => None,
        }
    }

    /// The receive step, then the channel reads.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let received = self.receive();
        self.publish(telemetry)?;
        Ok(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_first_creation_failure_raises_before_game_errors_or_later_socket_calls() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Failure(Arc<AtomicUsize>);
        impl DatagramSocket for Failure {
            fn reopen(&self) -> std::io::Result<()> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("creation denied")))
            }
            fn close(&self) {
                self.0.fetch_add(100, Ordering::Relaxed);
            }
            fn receive(&self, _: usize) -> std::io::Result<Vec<u8>> {
                panic!("creation failure precedes receive")
            }
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut source =
            TduscSource::with_native_socket(Box::new(Failure(attempts.clone())), Arc::new(Forwarder::default()));
        let result = source.is_running();
        assert!(result.is_err(), "the original creation catch tries None.close and raises");
        let error = result.unwrap_err();
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error));
        assert!(error.to_string().contains("'NoneType' object has no attribute 'close'"));
        assert!(source.game_errors().is_empty(), "only bind/settimeout adds a game error");
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(source.packet.is_none());

        let compatibility_attempts = Arc::new(AtomicUsize::new(0));
        let mut compatibility =
            TduscSource::with_socket(Box::new(Failure(compatibility_attempts.clone())), Arc::new(Forwarder::default()));
        assert!(
            !compatibility.is_running().unwrap(),
            "the public numeric fake constructor keeps its existing behavior"
        );
        assert_eq!(compatibility_attempts.load(Ordering::Relaxed), 101);
        assert!(compatibility.game_errors().contains("creation denied"));
        super::super::dirt::assert_native_creation_ends_dispatcher(TduscSource::with_native_socket(
            Box::new(Failure(Arc::new(AtomicUsize::new(0)))),
            Arc::new(Forwarder::default()),
        ));
    }

    #[test]
    fn native_later_creation_failures_keep_packet_and_do_not_add_game_errors() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Staged {
            attempts: Arc<AtomicUsize>,
            closes: Arc<AtomicUsize>,
            socket: ScriptedSocket,
        }
        impl DatagramSocket for Staged {
            fn reopen(&self) -> std::io::Result<()> {
                let step = self.attempts.fetch_add(1, Ordering::Relaxed);
                match step {
                    0 => Err(std::io::Error::other("bind denied")),
                    1 | 3 => {
                        Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("creation denied")))
                    }
                    2 => Err(std::io::Error::other("settimeout denied")),
                    _ => self.socket.reopen(),
                }
            }
            fn close(&self) {
                self.closes.fetch_add(1, Ordering::Relaxed);
                self.socket.close();
            }
            fn receive(&self, length: usize) -> std::io::Result<Vec<u8>> {
                self.socket.receive(length)
            }
        }
        let socket = ScriptedSocket::new();
        socket.push_timeout();
        socket.push(b"probe");
        let data = encode(Layout::Wrc, &[(field::SPEED, 25.0), (field::RPM, 512.5)]);
        socket.push(&data);
        let attempts = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let mut source = TduscSource::with_native_socket(
            Box::new(Staged { attempts: attempts.clone(), closes: closes.clone(), socket }),
            Arc::new(Forwarder::default()),
        );
        source.packet = MotionPacket::decode(&data, Layout::Wrc);
        let previous_packet = source.packet;
        let previous_heave = source.heave.lock().unwrap().clone();
        source.game_errors.record(&std::io::Error::other("earlier error"));
        for step in 0..5 {
            assert!(!source.is_running().unwrap(), "setup/receive step {step}");
            assert!(source.socket_assigned, "bind was attempted before every later creation error");
            assert_eq!(source.packet, previous_packet);
            assert_eq!(*source.heave.lock().unwrap(), previous_heave);
            assert!(!source.game_errors().contains("creation denied"));
            assert!(source.game_errors().contains("earlier error"));
        }
        assert_eq!(closes.load(Ordering::Relaxed), 5);
        assert_eq!(
            source.game_errors().iter().map(String::as_str).collect::<Vec<_>>(),
            ["bind denied", "earlier error", "settimeout denied"]
        );
        assert!(source.is_running().unwrap());
        assert_eq!(attempts.load(Ordering::Relaxed), 6);
        assert!(source.is_running().unwrap(), "connected checks skip socket setup");
        assert_eq!(attempts.load(Ordering::Relaxed), 6);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert!(source.game_errors().is_empty(), "the successful receive clears the shared set");
        assert_eq!(source.packet, previous_packet);
    }

    #[test]
    fn native_adapter_receives_without_heave_and_reads_it_once() {
        use crate::derive::{Listener, SourceListener};
        let (source, socket) = connected();
        socket.push(&encode(
            Layout::Wrc,
            &[
                (field::TIME, 0.5),
                (field::RPM, 500.0),
                (field::SPEED, 30.0),
                (field::RX, 1.0),
                (field::FY, 1.0),
                (field::ZV, -1.0),
            ],
        ));
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.failure_point(), None);
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Str("500".into())));
        assert_eq!(listener.native_channel("vertical_acceleration"), Some(LegacyValue::Float(-2.0)));
        assert_eq!(listener.native_channel("vertical_acceleration"), Some(LegacyValue::Float(-0.0)));
    }
    use crate::udp_racing::codemasters::packet::{encode, field};
    use crate::udp_racing::listener_output::zero_dispatcher_channels;
    use crate::udp_racing::socket::scripted::ScriptedSocket;
    use crate::udp_racing::unhandled::assert_ends;

    fn connected() -> (TduscSource, ScriptedSocket) {
        let socket = ScriptedSocket::new();
        let mut source = TduscSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default()));
        socket.push(b"probe");
        assert!(source.is_running().unwrap());
        (source, socket)
    }

    #[test]
    fn channels_of_a_moving_car() {
        // Channel values.
        let (mut source, socket) = connected();
        let packet = encode(
            Layout::Wrc,
            &[
                (field::TIME, 0.25),
                (field::RPM, 4321.9),
                (field::MAX_RPM, 7000.0),
                (field::SPEED, 33.3),
                (field::GEAR, 5.0),
                (field::SUSP_POS_RR, 50.0),
                (field::GFORCE_LON, 0.5),
                (field::GFORCE_LAT, 0.25),
                (field::STEER, -0.5),
                (field::RX, -1.0),
                (field::FY, -1.0),
                (field::XV, 3.0),
                (field::ZV, 1.0),
            ],
        );
        socket.push(&packet);
        let mut t = Telemetry::off();
        assert!(source.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.speed, t.gear), (4321.0, 7000.0, 33.0, 5.0));
        assert_eq!(t.suspension_travel_rr, 50.0 / 10000.0);
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.steering_wheel_angle), (-0.5, 0.25, -0.5));
        // r = (-1,0,0), f = (0,-1,0) negate to the identity matrix: all angles 0.
        assert_eq!((t.pitch, t.yaw, t.roll.abs()), (0.0, 0.0, 0.0));
        assert_eq!(t.lateral_velocity, 3.0);
        // up = (fy*rz - fz*ry, fz*rx - fx*rz, fx*ry - fy*rx) = (0, 0, -1): vs = -1, negated result.
        assert_eq!(t.vertical_acceleration, -(-1.0 / 0.25));
    }

    #[test]
    fn stationary_test_ignores_pitch() {
        // Stationary test: rpm and speed truncate to 0 -> 0 for every channel.
        let (mut source, socket) = connected();
        socket.push(&encode(Layout::Wrc, &[(field::RPM, 0.7), (field::GEAR, 3.0), (field::RX, 1.0), (field::FZ, 1.0)]));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!((t.gear, t.max_rpm), (0.0, 0.0));
    }

    #[test]
    fn nan_rpm_stores_only_location_and_a_zero_lateral_velocity() {
        // A NaN has no integer value, so every read fails; the dispatcher keeps
        // the vehicle name, stores location 0 and lateral_velocity 0, then its top-speed update
        // fails on the speed read and ends the pass.
        let (mut source, socket) = connected();
        let mut t = Telemetry::off();
        (t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag) = (11.0, 22.0, 33.0, 44.0, 77.0, 3.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        socket.push(&encode(Layout::Wrc, &[(field::RPM, f32::NAN), (field::RX, -1.0), (field::XV, 3.0)]));
        assert_ends(source.poll(&mut t));
        assert_eq!((t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag), (11.0, 22.0, 33.0, 44.0, 0.0, 3.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("Earlier car", ""));
    }

    #[test]
    fn poll_before_the_first_packet_reads_zero_everywhere() {
        // No packet yet: every channel reads 0.
        let (mut source, socket) = connected();
        let mut t = Telemetry::off();
        (t.pitch, t.rpm, t.wheel_slip_lf, t.location) = (11.0, 44.0, 0.4, "Monza".into());
        socket.push_timeout();
        source.poll(&mut t).unwrap();
        let mut zero = Telemetry::off();
        zero_dispatcher_channels(&mut zero);
        assert_eq!(t, zero);
    }

    #[test]
    fn unmapped_channels_read_zero_on_every_tick() {
        // Every name the listener does not answer reads 0.
        let (mut source, socket) = connected();
        socket.push(&encode(
            Layout::Wrc,
            &[(field::RPM, 900.0), (field::SPEED, 10.0), (field::RX, -1.0), (field::FY, -1.0)],
        ));
        let mut t = Telemetry::off();
        (t.raw_flag, t.wheel_slip_lf, t.longitudinal_velocity) = (3.0, 0.4, 2.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        source.poll(&mut t).unwrap();
        assert_eq!((t.raw_flag, t.wheel_slip_lf, t.longitudinal_velocity, t.rpm), (0.0, 0.0, 0.0, 900.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("", ""));
    }

    #[test]
    fn nan_speed_of_a_running_engine_stores_only_lateral_velocity() {
        // A non-zero rpm stops the stationary test; the NaN speed read then fails in the top-speed update after
        // the dispatcher stored lateral_velocity = -(rx*xv + ry*yv + rz*zv).
        let (mut source, socket) = connected();
        let mut t = Telemetry::off();
        t.rpm = 44.0;
        socket.push(&encode(
            Layout::Wrc,
            &[(field::RPM, 900.0), (field::SPEED, f32::NAN), (field::RX, -1.0), (field::XV, 3.0)],
        ));
        t.location = "Monza".into();
        assert_ends(source.poll(&mut t));
        assert_eq!((t.rpm, t.lateral_velocity, t.location.as_str()), (44.0, 3.0, ""));
    }

    #[test]
    fn nan_frame_component_gives_a_ninety_degree_yaw() {
        // The NaN arcsine argument fails `t < 1.0` and becomes 1.0.
        let (mut source, socket) = connected();
        socket.push(&encode(
            Layout::Wrc,
            &[(field::RPM, 900.0), (field::SPEED, 10.0), (field::RX, f32::NAN), (field::FY, -1.0)],
        ));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!(t.yaw, 1.0f64.asin() * (180.0 / std::f64::consts::PI));
    }

    #[test]
    fn other_lengths_disconnect() {
        // A datagram that is not 280 bytes marks not connected and the socket is closed.
        let (mut source, socket) = connected();
        socket.push(&[0u8; 264]);
        let mut t = Telemetry::off();
        assert!(!source.poll(&mut t).unwrap());
        assert!(!socket.is_open());
        assert!(!source.is_running().unwrap());
    }

    #[test]
    fn a_bind_failure_is_a_game_error_until_any_datagram_arrives() {
        // A bind failure adds its text to the game errors; the receive step clears them
        // after both branches of the length test.
        let socket = ScriptedSocket::new();
        let mut source = TduscSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default()));
        socket.refuse_bind(true);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().len(), 1);
        socket.refuse_bind(false);
        socket.push(b"probe");
        assert!(source.is_running().unwrap());
        socket.push(b"short");
        let mut t = Telemetry::off();
        assert!(!source.poll(&mut t).unwrap());
        assert!(source.game_errors().is_empty());
    }
}

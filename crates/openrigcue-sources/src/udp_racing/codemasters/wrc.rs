//! WRC Generations (listener `wrc`).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::kinematics::{frame_yaw, lateral_speed, roll_degrees, vertical_speed, HeaveEstimator, HeaveRule};
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
const TIMEOUT: Duration = Duration::from_millis(100);
/// Receive buffer size.
const RECEIVE_SIZE: usize = 512;
/// `max_rpm` when reading it fails.
const FALLBACK_MAX_RPM: f64 = 20000.0;

/// The WRC Generations listener: every probe binds a new socket that the next receive step reads from.
pub struct WrcSource {
    socket: Box<dyn DatagramSocket>,
    native_socket: bool,
    socket_assigned: bool,
    forwarder: Arc<Forwarder>,
    packet: Option<MotionPacket>,
    heave: Mutex<HeaveEstimator>,
    /// This listener's share of the game errors.
    game_errors: GameErrors,
}

impl WrcSource {
    /// Listen on `127.0.0.1:<settings.port>`.
    pub fn new(settings: &ListenerSettings) -> Self {
        let socket = UdpEndpoint::new(Endpoint::loopback(settings.port, TIMEOUT));
        WrcSource::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
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
        WrcSource {
            socket,
            native_socket: false,
            socket_assigned: false,
            forwarder,
            packet: None,
            heave: Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero)),
            game_errors: GameErrors::default(),
        }
    }

    /// The texts this listener added to the game errors (a failed bind or timeout setting in
    /// the liveness test), emptied when the receive step takes in a datagram.
    pub fn game_errors(&self) -> &std::collections::BTreeSet<String> {
        self.game_errors.messages()
    }

    /// The receive step: one datagram; only a 280-byte packet (70 floats) is parsed.
    fn receive(&mut self) -> bool {
        match self.socket.receive(RECEIVE_SIZE) {
            Ok(datagram) => {
                self.forwarder.forward(&datagram);
                // The game errors are cleared after the length test.
                self.game_errors.clear();
                let parsed = MotionPacket::decode(&datagram, Layout::Wrc);
                let fresh = parsed.is_some();
                if parsed.is_some() {
                    self.packet = parsed;
                }
                fresh
            }
            Err(_) => {
                self.socket.close();
                false
            }
        }
    }

    /// Writes every registry channel the listener answers.
    fn publish(&mut self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        // No packet yet: nothing fails, every channel reads 0.
        let Some(p) = self.packet else {
            publish_unmapped_as_zero(t, CarName::Blank);
            return Ok(());
        };
        match stationary(&p, GuardRule::RpmTimesTenSpeedPitch) {
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
        let rpm = python_int(p.rpm * 10.0).unwrap_or(t.rpm);
        let gear = python_int(p.gear).unwrap_or(t.gear);
        publish_unmapped_as_zero(t, CarName::Blank);
        t.speed = speed;
        t.rpm = rpm;
        t.max_rpm = p.session.and_then(|s| python_int(s.max_rpm * 10.0)).unwrap_or(FALLBACK_MAX_RPM);
        t.gear = gear;
        t.suspension_travel_lf = p.suspension_position.front_left / 2.5;
        t.suspension_travel_lr = p.suspension_position.rear_left / 2.5;
        t.suspension_travel_rf = p.suspension_position.front_right / 2.5;
        t.suspension_travel_rr = p.suspension_position.rear_right / 2.5;
        t.suspension_velocity_lr = p.suspension_velocity.rear_left;
        t.suspension_velocity_rr = p.suspension_velocity.rear_right;
        t.suspension_velocity_lf = p.suspension_velocity.front_left;
        t.suspension_velocity_rf = p.suspension_velocity.front_right;
        t.steering_wheel_angle = p.steer;
        t.longitudinal_acceleration = -p.gforce_lon;
        t.lateral_acceleration = p.gforce_lat;
        // Roll from `rz`; a failed roll makes the dispatcher store 0.
        t.roll = roll_degrees(p.frame.right[2]).unwrap_or(0.0);
        t.pitch = 0.0;
        t.yaw = frame_yaw(&p.frame);
        t.lateral_velocity = lateral_velocity;
        t.vertical_acceleration =
            self.heave.get_mut().unwrap_or_else(PoisonError::into_inner).next(p.time, vertical_speed(&p.frame)) * 2.0;
        Ok(())
    }

    /// The liveness test: bind a new socket every call and wait up to 0.1 s for a datagram;
    /// the socket stays open for the receive step.
    fn check_status(&mut self) -> Result<bool, SourceError> {
        if let Err(error) = self.socket.reopen() {
            if self.native_socket && socket_creation_failed(&error) {
                if !self.socket_assigned {
                    return Err(
                        DispatcherEnds::new("AttributeError: 'NoneType' object has no attribute 'close'").into()
                    );
                }
                self.socket.close();
                return Ok(false);
            }
            self.socket_assigned = true;
            self.game_errors.record(&error);
            self.socket.close();
            return Ok(false);
        }
        self.socket_assigned = true;
        Ok(match self.socket.receive(RECEIVE_SIZE) {
            Ok(datagram) => {
                self.forwarder.forward(&datagram);
                true
            }
            Err(_) => {
                self.socket.close();
                false
            }
        })
    }
}

impl Source for WrcSource {
    fn id(&self) -> &'static str {
        "wrc"
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        self.check_status()
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }
    fn read_native(&self, name: &str) -> NativeRead {
        super::native::read(self.packet, &self.heave, super::native::Kind::Wrc, name)
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
            WrcSource::with_native_socket(Box::new(Failure(attempts.clone())), Arc::new(Forwarder::default()));
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
            WrcSource::with_socket(Box::new(Failure(compatibility_attempts.clone())), Arc::new(Forwarder::default()));
        assert!(
            !compatibility.is_running().unwrap(),
            "the public numeric fake constructor keeps its existing behavior"
        );
        assert_eq!(compatibility_attempts.load(Ordering::Relaxed), 101);
        assert!(compatibility.game_errors().contains("creation denied"));
        super::super::dirt::assert_native_creation_ends_dispatcher(WrcSource::with_native_socket(
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
        let mut source = WrcSource::with_native_socket(
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
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert!(source.game_errors().is_empty(), "the successful receive clears the shared set");
        assert_eq!(source.packet, previous_packet);
    }
    use crate::udp_racing::codemasters::packet::{encode, field};
    use crate::udp_racing::listener_output::zero_dispatcher_channels;
    use crate::udp_racing::socket::scripted::ScriptedSocket;
    use crate::udp_racing::unhandled::assert_ends;

    fn source() -> (WrcSource, ScriptedSocket) {
        let socket = ScriptedSocket::new();
        (WrcSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default())), socket)
    }

    #[test]
    fn every_check_status_binds_a_new_socket() {
        // The liveness test has no connected flag.
        let (mut source, socket) = source();
        socket.push(b"a");
        socket.push(b"b");
        assert!(source.is_running().unwrap());
        assert!(source.is_running().unwrap());
        assert!(!source.is_running().unwrap());
        assert_eq!(socket.activity().reopened, 3);
        assert!(!socket.is_open());
    }

    /// Every dispatcher-read channel at 0, no car name, no location.
    fn all_zero() -> Telemetry {
        let mut zero = Telemetry::off();
        zero_dispatcher_channels(&mut zero);
        zero
    }

    #[test]
    fn only_280_byte_packets_are_parsed() {
        // Only a 280-byte datagram is parsed. Nothing parsed yet, so every channel
        // reads 0.
        let (mut source, socket) = source();
        socket.push(b"probe");
        source.is_running().unwrap();
        socket.push(&[0u8; 256]);
        let mut t = Telemetry::off();
        (t.rpm, t.raw_flag, t.vehicle) = (44.0, 3.0, "Earlier car".into());
        assert!(!source.poll(&mut t).unwrap());
        assert_eq!(t, all_zero());
    }

    #[test]
    fn channels_of_a_moving_car() {
        // Channels: /2.5 travel, -gforce_lon, roll from rz, pitch 0, -lateral, heave * 2.
        let (mut source, socket) = source();
        socket.push(b"probe");
        source.is_running().unwrap();
        let packet = encode(
            Layout::Wrc,
            &[
                (field::TIME, 0.5),
                (field::RPM, 300.0),
                (field::SPEED, 12.9),
                (field::GEAR, 2.0),
                (field::MAX_RPM, 800.0),
                (field::SUSP_POS_FL, 5.0),
                (field::GFORCE_LON, 0.25),
                (field::GFORCE_LAT, -1.0),
                (field::STEER, 0.75),
                (field::RX, 0.6),
                (field::RZ, 0.5),
                (field::FY, 1.0),
                (field::XV, 1.0),
                (field::ZV, 2.0),
            ],
        );
        socket.push(&packet);
        let mut t = Telemetry::off();
        assert!(source.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (3000.0, 8000.0, 2.0, 12.0));
        assert_eq!(t.suspension_travel_lf, 2.0);
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration, t.steering_wheel_angle), (-0.25, -1.0, 0.75));
        assert!((t.roll - 30.0).abs() < 1e-9);
        assert_eq!(t.pitch, 0.0);
        assert_eq!(t.yaw, yaw_reference(0.6, 0.5));
        assert_eq!(t.lateral_velocity, -(f64::from(0.6f32) * 1.0 + 0.5 * 2.0));
        // up = (fy*rz - fz*ry, fz*rx - fx*rz, fx*ry - fy*rx) = (0.5, 0, -0.6): vs = 0.5*1 - 0.6*2
        let vs = 0.5 * 1.0 + -f64::from(0.6f32) * 2.0;
        assert_eq!(t.vertical_acceleration, vs / 0.5 * 2.0);
    }

    fn yaw_reference(rx: f32, rz: f32) -> f64 {
        crate::udp_racing::codemasters::kinematics::yaw_degrees(f64::from(rx), f64::from(rz))
    }

    #[test]
    fn nan_rpm_stores_only_location_and_a_zero_lateral_velocity() {
        // A NaN has no integer value, so every read fails; the dispatcher keeps
        // the vehicle name, stores location 0 and lateral_velocity 0, then its top-speed update
        // fails on the speed read and ends the pass.
        let (mut source, socket) = source();
        let mut t = Telemetry::off();
        (t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag) = (11.0, 22.0, 33.0, 44.0, 77.0, 3.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        socket.push(b"probe");
        source.is_running().unwrap();
        socket.push(&encode(Layout::Wrc, &[(field::RPM, f32::NAN), (field::RX, 1.0), (field::XV, 2.0)]));
        assert_ends(source.poll(&mut t));
        assert_eq!((t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag), (11.0, 22.0, 33.0, 44.0, 0.0, 3.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("Earlier car", ""));
    }

    #[test]
    fn poll_before_the_first_packet_reads_zero_everywhere() {
        // No packet yet: every channel reads 0.
        let (mut source, socket) = source();
        let mut t = Telemetry::off();
        (t.pitch, t.rpm, t.wheel_slip_lf, t.location) = (11.0, 44.0, 0.4, "Monza".into());
        socket.push(b"probe");
        source.is_running().unwrap();
        assert!(!source.poll(&mut t).unwrap());
        assert_eq!(t, all_zero());
    }

    #[test]
    fn unmapped_channels_read_zero_on_every_tick() {
        // Every name the listener does not answer reads 0.
        let (mut source, socket) = source();
        socket.push(b"probe");
        source.is_running().unwrap();
        socket.push(&encode(Layout::Wrc, &[(field::RPM, 300.0), (field::FY, 1.0), (field::RX, 1.0)]));
        let mut t = Telemetry::off();
        (t.raw_flag, t.wheel_slip_lf, t.vertical_velocity) = (3.0, 0.4, 2.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        source.poll(&mut t).unwrap();
        assert_eq!((t.raw_flag, t.wheel_slip_lf, t.vertical_velocity, t.rpm), (0.0, 0.0, 0.0, 3000.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("", ""));
    }

    #[test]
    fn receive_failure_closes_but_keeps_channels() {
        // A failed receive closes the socket; channels still answer from the previous packet.
        let (mut source, socket) = source();
        socket.push(b"probe");
        source.is_running().unwrap();
        socket.push(&encode(Layout::Wrc, &[(field::RPM, 300.0), (field::FY, 1.0), (field::RX, 1.0)]));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        t.rpm = 0.0;
        socket.push(b"probe");
        assert!(source.is_running().unwrap());
        assert!(!source.poll(&mut t).unwrap());
        assert!(!socket.is_open());
        assert_eq!(t.rpm, 3000.0);
    }

    #[test]
    fn a_bind_failure_is_a_game_error_until_any_datagram_arrives() {
        // A failed liveness test adds its text to the game errors; the receive step clears them.
        let (mut source, socket) = source();
        socket.refuse_bind(true);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().len(), 1);
        socket.refuse_bind(false);
        socket.push(b"probe");
        assert!(source.is_running().unwrap());
        socket.push(b"other length");
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert!(source.game_errors().is_empty());
    }
}

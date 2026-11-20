//! GRID 2, GRID (2019) and GRID Legends (listener `grid`).

use std::sync::{Arc, Mutex, PoisonError};

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::kinematics::{
    frame_pitch, frame_yaw, lateral_speed, roll_degrees, vertical_speed, HeaveEstimator, HeaveRule,
};
use super::packet::MotionPacket;
use super::probe::{parse_extradata, ProbeReceiver};
use super::{publish_speed_unreadable, publish_unmapped_as_zero, CarName};
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::listener_output::python_int;
use crate::udp_racing::net::timeout_secs;
use crate::udp_racing::socket::{DatagramSocket, Endpoint, ImportedPort, ListenerSettings, UdpEndpoint};
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{NativeRead, Source, SourceError};

/// Receive timeout: 1/15 s (binary64 0x3fb1111111111111), in seconds.
const TIMEOUT_SECONDS: f64 = 1.0 / 15.0;
/// Speed factor 0.6214 (binary64 0x3fe3e28240b78034).
const SPEED_FACTOR: f64 = 0.6214;

/// The GRID listener: a fresh loopback socket on every probe.
pub struct GridSource {
    receiver: ProbeReceiver,
    packet: Option<MotionPacket>,
    heave: Mutex<HeaveEstimator>,
}

impl GridSource {
    /// Listen on `127.0.0.1:<settings.port>`.
    pub fn new(settings: &ListenerSettings) -> Self {
        let socket = UdpEndpoint::new(Endpoint::loopback(settings.port, timeout_secs(TIMEOUT_SECONDS)));
        GridSource::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Keeps the configured port as any integer; bind checks its range on every probe.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        let socket =
            UdpEndpoint::from_imported_port(std::net::Ipv4Addr::LOCALHOST, port, timeout_secs(TIMEOUT_SECONDS));
        Self::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Use the given socket and forwarder (tests).
    pub fn with_socket(socket: Box<dyn DatagramSocket>, forwarder: Arc<Forwarder>) -> Self {
        GridSource {
            receiver: ProbeReceiver::new(socket, forwarder, None),
            packet: None,
            heave: Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero)),
        }
    }

    fn receive(&mut self) -> bool {
        let fresh = self.receiver.data().and_then(parse_extradata);
        self.packet = fresh.or(self.packet);
        fresh.is_some()
    }

    /// Writes every registry channel the listener answers.
    fn publish(&mut self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        // No packet yet: every mapped read fails; `car_name` is unmapped and answers 0.
        let Some(p) = self.packet else { return publish_speed_unreadable(t, 0.0, CarName::Blank) };
        let angular = p.extended.map(|e| e.angular_velocity);
        let lateral_velocity = angular.map_or_else(|| lateral_speed(&p.frame), |a| a[0] * 10.0);
        let Some(speed) = python_int(p.speed * SPEED_FACTOR) else {
            return publish_speed_unreadable(t, lateral_velocity, CarName::Blank);
        };
        // A failed integer conversion leaves the channel as it was (no registry default).
        let rpm = python_int(p.rpm * 10.0).unwrap_or(t.rpm);
        let max_rpm = p.session.and_then(|s| python_int(s.max_rpm * 10.0)).unwrap_or(t.max_rpm);
        let gear = python_int(p.gear).unwrap_or(t.gear);
        publish_unmapped_as_zero(t, CarName::Blank);
        t.speed = speed;
        t.rpm = rpm;
        t.max_rpm = max_rpm;
        t.gear = gear;
        t.suspension_travel_lf = p.suspension_position.front_left / 1000.0;
        t.suspension_travel_lr = p.suspension_position.rear_left / 1000.0;
        t.suspension_travel_rf = p.suspension_position.front_right / 1000.0;
        t.suspension_travel_rr = p.suspension_position.rear_right / 1000.0;
        t.suspension_velocity_lr = p.suspension_velocity.rear_left;
        t.suspension_velocity_rr = p.suspension_velocity.rear_right;
        t.suspension_velocity_lf = p.suspension_velocity.front_left;
        t.suspension_velocity_rf = p.suspension_velocity.front_right;
        t.steering_wheel_angle = p.steer * 10.0;
        t.vertical_acceleration = match p.extended {
            Some(extended) => extended.gforce_vert,
            None => self.heave.get_mut().unwrap_or_else(PoisonError::into_inner).next(p.time, vertical_speed(&p.frame)),
        };
        t.lateral_acceleration = p.gforce_lat;
        t.longitudinal_acceleration = p.gforce_lon * 10.0;
        t.longitudinal_velocity = angular.map_or(0.0, |a| a[2]);
        t.vertical_velocity = angular.map_or(0.0, |a| a[1]);
        t.lateral_velocity = lateral_velocity;
        // A failed roll computation makes the dispatcher store 0 for roll.
        t.roll = roll_degrees(p.frame.right[1]).map_or(0.0, |roll| -roll);
        t.pitch = frame_pitch(&p.frame);
        t.yaw = frame_yaw(&p.frame);
        Ok(())
    }
}

impl Source for GridSource {
    fn id(&self) -> &'static str {
        "grid"
    }

    /// The liveness test.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.receiver.probe())
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }
    fn read_native(&self, name: &str) -> NativeRead {
        super::native::read(self.packet, &self.heave, super::native::Kind::Grid, name)
    }
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            _ => None,
        }
    }

    /// The receive step (parse the stored datagram) followed by the channel reads.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let received = self.receive();
        self.publish(telemetry)?;
        Ok(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::codemasters::packet::{encode, field, Layout};
    use crate::udp_racing::socket::scripted::ScriptedSocket;
    use crate::udp_racing::unhandled::assert_ends;

    fn run(datagram: &[u8]) -> Telemetry {
        let socket = ScriptedSocket::new();
        let mut source = GridSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default()));
        socket.push(datagram);
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        // A tick whose speed read fails also ends the dispatcher; its channels are what is checked.
        if let Err(error) = source.poll(&mut t) {
            assert!(crate::udp_racing::unhandled::ends_dispatcher(&error), "{error}");
        }
        t
    }

    #[test]
    fn timeout_is_one_fifteenth_of_a_second() {
        assert_eq!(TIMEOUT_SECONDS.to_bits(), 0x3fb1_1111_1111_1111);
    }

    #[test]
    fn extradata3_channels() {
        // Channels: no stationary guard, all four suspension positions divided.
        let mut packet = encode(
            Layout::Extradata3,
            &[
                (field::TIME, 0.5),
                (field::RPM, 0.0),
                (field::MAX_RPM, 700.5),
                (field::GEAR, 4.0),
                (field::SUSP_POS_FL, 1.0),
                (field::SUSP_POS_RL, 2.0),
                (field::SUSP_POS_FR, 3.0),
                (field::SUSP_POS_RR, 4.0),
                (field::STEER, -0.1),
                (field::GFORCE_LAT, 1.5),
                (field::GFORCE_LON, -0.5),
                (field::RY, -0.5),
                (field::RX, 0.8),
                (field::XV, 2.0),
            ],
        );
        packet.extend_from_slice(&[0; 8]);
        let t = run(&packet);
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (0.0, 7005.0, 4.0, 0.0));
        assert_eq!(
            [t.suspension_travel_lf, t.suspension_travel_lr, t.suspension_travel_rf, t.suspension_travel_rr],
            [1.0 / 1000.0, 2.0 / 1000.0, 3.0 / 1000.0, 4.0 / 1000.0]
        );
        assert_eq!(t.steering_wheel_angle, f64::from(-0.1f32) * 10.0);
        assert_eq!((t.lateral_acceleration, t.longitudinal_acceleration), (1.5, -5.0));
        assert!((t.roll - 30.0).abs() < 1e-9, "roll is negated: {}", t.roll);
        assert_eq!(t.lateral_velocity, f64::from(0.8f32) * 2.0);
        assert_eq!((t.vertical_velocity, t.longitudinal_velocity), (0.0, 0.0));
        assert_eq!(t.vertical_acceleration, 0.0);
    }

    #[test]
    fn udp_data_packet_uses_gforce_vert_and_angular_velocity() {
        let t = run(&encode(
            Layout::Extended,
            &[(field::GFORCE_VERT, -0.75), (field::ANG_VEL_X, 0.5), (field::ANG_VEL_Y, 2.0), (field::ANG_VEL_Z, -3.0)],
        ));
        assert_eq!(
            (t.vertical_acceleration, t.lateral_velocity, t.vertical_velocity, t.longitudinal_velocity),
            (-0.75, 5.0, 2.0, -3.0)
        );
    }

    #[test]
    fn poll_before_the_first_packet_stores_only_name_location_and_lateral_velocity() {
        // There is no packet at start; a 100-byte datagram parses nothing, so every mapped
        // read fails while car_name and track_loc read 0: the dispatcher stores
        // those and lateral_velocity 0, then its top-speed update fails on the speed read and ends the pass.
        let mut t = Telemetry::off();
        (t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag) = (11.0, 22.0, 33.0, 44.0, 77.0, 3.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        let socket = ScriptedSocket::new();
        let mut source = GridSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default()));
        socket.push(&[1u8; 100]);
        assert!(source.is_running().unwrap());
        assert_ends(source.poll(&mut t));
        assert_eq!((t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag), (11.0, 22.0, 33.0, 44.0, 0.0, 3.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("", ""));
    }

    #[test]
    fn nan_speed_stores_only_lateral_velocity() {
        let t = run(&encode(
            Layout::Extended,
            &[(field::SPEED, f32::NAN), (field::RPM, 90.0), (field::ANG_VEL_X, 0.5), (field::RY, 0.5)],
        ));
        assert_eq!((t.rpm, t.roll, t.lateral_velocity), (0.0, 0.0, 5.0));
    }

    #[test]
    fn unmapped_channels_read_zero_on_every_tick() {
        // Every name the listener does not answer reads 0.
        let socket = ScriptedSocket::new();
        let mut source = GridSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default()));
        socket.push(&encode(Layout::Extended, &[(field::RPM, 90.0)]));
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        (t.raw_flag, t.wheel_slip_lf, t.lap) = (3.0, 0.4, 2.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        source.poll(&mut t).unwrap();
        assert_eq!((t.raw_flag, t.wheel_slip_lf, t.lap, t.rpm), (0.0, 0.0, 0.0, 900.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("", ""));
    }

    #[test]
    fn receive_data_parses_the_datagram_check_status_received() {
        // The liveness test receives the datagram, the receive step parses it; the receive step
        // receives nothing itself.
        let socket = ScriptedSocket::new();
        let mut source = GridSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default()));
        socket.push(&encode(Layout::Extended, &[(field::RPM, 10.0)]));
        assert!(source.is_running().unwrap());
        socket.push(&encode(Layout::Extended, &[(field::RPM, 20.0)]));
        let mut t = Telemetry::off();
        for _ in 0..2 {
            assert!(source.poll(&mut t).unwrap());
            assert_eq!(t.rpm, 100.0);
        }
        assert_eq!(socket.activity().reopened, 1);
        assert_eq!(socket.pending(), 1);
    }
}

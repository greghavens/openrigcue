//! F1 2013, 2014, 2015, 2016 and 2017 (listener `f1`).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::kinematics::{
    frame_pitch, frame_yaw, lateral_speed, roll_degrees, vertical_speed, HeaveEstimator, HeaveRule,
};
use super::packet::MotionPacket;
use super::probe::{parse_extradata, ProbeReceiver};
use super::{publish_speed_unreadable, publish_unmapped_as_zero, CarName};
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::listener_output::{python_index, python_int};
use crate::udp_racing::parameters::GameErrors;
use crate::udp_racing::socket::{DatagramSocket, Endpoint, ImportedPort, ListenerSettings, UdpEndpoint};
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{NativeRead, Source, SourceError};

/// Receive timeout.
const TIMEOUT: Duration = Duration::from_millis(200);
/// Speed factor 0.6214 (binary64 0x3fe3e28240b78034).
const SPEED_FACTOR: f64 = 0.6214;

/// The 2016 team table (13 slots, indexed by `team_info`), used when the packet has no `era` or `era == -1`.
const VEHICLES_2016: [Option<&str>; 13] = [
    Some("Redbull"),
    Some("Ferrari"),
    Some("McLaren"),
    Some("Lotus"),
    Some("Mercedes"),
    Some("Sauber"),
    Some("Force India"),
    Some("Williams"),
    Some("Toro Rosso"),
    Some("Caterham"),
    Some("Marussia"),
    Some("Haas"),
    Some("Manor"),
];
/// The 2017 team table (12 slots), used when `era == 2017`.
const VEHICLES_2017: [Option<&str>; 12] = [
    Some("Redbull"),
    Some("Ferrari"),
    Some("McLaren"),
    Some("Renault"),
    Some("Mercedes"),
    Some("Sauber"),
    Some("Force India"),
    Some("Williams"),
    Some("Toro Rosso"),
    None,
    None,
    Some("Haas"),
];
/// The 2017 classic-car table (13 slots), used when `era == 1980`.
const VEHICLES_2017_CLASSIC: [Option<&str>; 13] = [
    Some("Williams 1992"),
    Some("McLaren 1988"),
    Some("McLaren 2008"),
    Some("Ferrari 2004"),
    Some("Ferrari 1995"),
    Some("Ferrari 2007"),
    Some("McLaren 1998"),
    Some("Williams 1996"),
    Some("Renault 2006"),
    None,
    Some("Ferrari 2002"),
    Some("Redbull 2010"),
    Some("McLaren 1991"),
];

/// The `car_name` channel: the era is the packet's `era` as an integer (-1
/// when the packet has no `era` or it is not finite); era -1 looks up the 2016 table by `team_info`
/// and answers 0 on any failure; era 2017 / 1980 look up the 2017 /
/// 2017 classic table, where a NaN or out-of-range `team_info` is a failed read; any other era answers 0.
/// A negative index counts from the end of the table; an empty table slot answers `None`.
pub fn car_name(packet: &MotionPacket) -> CarName {
    let era = packet.extended.and_then(|e| python_int(e.era)).unwrap_or(-1.0);
    let (table, failure): (&[Option<&'static str>], CarName) = if era == -1.0 {
        (&VEHICLES_2016, CarName::Blank)
    } else if era == 2017.0 {
        (&VEHICLES_2017, CarName::Raised)
    } else if era == 1980.0 {
        (&VEHICLES_2017_CLASSIC, CarName::Raised)
    } else {
        return CarName::Blank;
    };
    let slot = packet
        .session
        .and_then(|session| python_int(session.team_info))
        .and_then(|team| python_index(team as i64, table.len()));
    match slot {
        Some(index) => table[index].map_or(CarName::Blank, CarName::Named),
        None => failure,
    }
}

pub(super) fn native_car_name(packet: Option<MotionPacket>) -> Option<LegacyValue> {
    let Some(packet) = packet else { return Some(LegacyValue::Int(0)) };
    let era = packet.extended.and_then(|e| python_int(e.era)).unwrap_or(-1.0);
    let table: &[Option<&str>] = if era == -1.0 {
        &VEHICLES_2016
    } else if era == 2017.0 {
        &VEHICLES_2017
    } else if era == 1980.0 {
        &VEHICLES_2017_CLASSIC
    } else {
        return Some(LegacyValue::Int(0));
    };
    let slot =
        packet.session.and_then(|s| python_int(s.team_info)).and_then(|team| python_index(team as i64, table.len()));
    match slot {
        Some(index) => Some(table[index].map_or(LegacyValue::None, |name| LegacyValue::Str(name.into()))),
        None if era == -1.0 => Some(LegacyValue::Int(0)),
        None => None,
    }
}

/// Which F1 (or GRID 2) game an open window title names, checked in table order.
/// This is the `game` channel.
pub fn game_from_windows<S: AsRef<str>>(windows: &[S]) -> Option<&'static str> {
    [
        ("F1 2013", "f12013"),
        ("F1 2014", "f12014"),
        ("F1 2015", "f12015"),
        ("F1 2016", "f12016"),
        ("F1 2017", "f12017"),
        ("GRID 2", "grid2"),
    ]
    .into_iter()
    .find(|(needle, _)| windows.iter().any(|w| w.as_ref().contains(needle)))
    .map(|(_, game)| game)
}

/// The F1 2013–2017 listener: a fresh socket on every probe, bound to all interfaces for consoles.
pub struct F1ClassicSource {
    receiver: ProbeReceiver,
    packet: Option<MotionPacket>,
    heave: Mutex<HeaveEstimator>,
}

impl F1ClassicSource {
    /// Listen on `0.0.0.0:<settings.port>`.
    pub fn new(settings: &ListenerSettings) -> Self {
        let socket = UdpEndpoint::new(Endpoint::any_interface(settings.port, TIMEOUT));
        F1ClassicSource::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Keeps the configured port as any integer; bind checks its range on every probe.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        let socket = UdpEndpoint::from_imported_port(std::net::Ipv4Addr::UNSPECIFIED, port, TIMEOUT);
        Self::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Use the given socket and forwarder (tests).
    pub fn with_socket(socket: Box<dyn DatagramSocket>, forwarder: Arc<Forwarder>) -> Self {
        F1ClassicSource {
            receiver: ProbeReceiver::new(socket, forwarder, Some(GameErrors::default())),
            packet: None,
            heave: Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero)),
        }
    }

    /// The texts this listener added to the game errors (a failed bind or timeout setting in
    /// the liveness test), emptied when the receive step parses a datagram.
    pub fn game_errors(&self) -> &std::collections::BTreeSet<String> {
        static NONE: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        self.receiver.game_errors.as_ref().map_or(&NONE, GameErrors::messages)
    }

    fn receive(&mut self) -> bool {
        let fresh = self.receiver.data().and_then(parse_extradata);
        if fresh.is_some() {
            if let Some(errors) = self.receiver.game_errors.as_mut() {
                errors.clear();
            }
        }
        self.packet = fresh.or(self.packet);
        fresh.is_some()
    }

    /// Writes every registry channel the listener answers.
    fn publish(&mut self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        // No packet yet: every read fails except `car_name`, whose era -1 lookup
        // answers 0 on failure.
        let Some(p) = self.packet else { return publish_speed_unreadable(t, 0.0, CarName::Blank) };
        let angular = p.extended.map(|e| e.angular_velocity);
        let lateral_velocity = angular.map_or_else(|| lateral_speed(&p.frame), |a| a[0] * 10.0);
        let Some(speed) = python_int(p.speed * SPEED_FACTOR) else {
            return publish_speed_unreadable(t, lateral_velocity, car_name(&p));
        };
        // A failed integer conversion leaves the channel as it was (no registry default).
        let rpm = python_int(p.rpm).unwrap_or(t.rpm);
        let max_rpm = p.session.and_then(|s| python_int(s.max_rpm)).unwrap_or(t.max_rpm);
        // The gear: truncate first, then subtract 1.
        let gear = python_int(p.gear).map_or(t.gear, |gear| gear - 1.0);
        publish_unmapped_as_zero(t, car_name(&p));
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
        t.roll = roll_degrees(p.frame.right[1]).unwrap_or(0.0);
        t.pitch = frame_pitch(&p.frame);
        t.yaw = frame_yaw(&p.frame);
        Ok(())
    }
}

impl Source for F1ClassicSource {
    fn id(&self) -> &'static str {
        "f1"
    }

    /// The liveness test.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.receiver.probe())
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }
    fn read_native(&self, name: &str) -> NativeRead {
        super::native::read(self.packet, &self.heave, super::native::Kind::F1, name)
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

    fn source() -> (F1ClassicSource, ScriptedSocket) {
        let socket = ScriptedSocket::new();
        (F1ClassicSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default())), socket)
    }

    fn run(source: &mut F1ClassicSource, socket: &ScriptedSocket, datagram: &[u8]) -> Telemetry {
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
    fn check_status_opens_and_closes_a_socket_per_call() {
        // Liveness: create, bind, set the timeout, receive, forward, close; the datagram is the answer.
        let (mut source, socket) = source();
        assert!(!source.is_running().unwrap());
        socket.push(b"");
        assert!(!source.is_running().unwrap(), "an empty datagram is falsy");
        socket.push(b"x");
        assert!(source.is_running().unwrap());
        let activity = socket.activity();
        assert_eq!((activity.reopened, activity.closed, activity.receives), (3, 3, vec![2048, 2048, 2048]));
        assert!(!socket.is_open());
    }

    #[test]
    fn udp_data_packet_channels() {
        // Channel values with the 304-byte F1 2016/2017 packet.
        let (mut source, socket) = source();
        let packet = encode(
            Layout::Extended,
            &[
                (field::RPM, 11250.7),
                (field::MAX_RPM, 12000.0),
                (field::GEAR, 0.0),
                (field::SPEED, 50.0),
                (field::SUSP_POS_FL, 10.0),
                (field::SUSP_POS_RR, 20.0),
                (field::STEER, 0.5),
                (field::GFORCE_VERT, 1.25),
                (field::GFORCE_LAT, 2.0),
                (field::GFORCE_LON, 0.3),
                (field::ANG_VEL_X, 0.2),
                (field::ANG_VEL_Y, -1.0),
                (field::ANG_VEL_Z, 3.0),
                (field::ERA, 2017.0),
                (field::TEAM_INFO, 3.0),
                (field::RY, 0.5),
            ],
        );
        let t = run(&mut source, &socket, &packet);
        assert_eq!((t.rpm, t.max_rpm, t.gear), (11250.0, 12000.0, -1.0));
        assert_eq!(t.speed, 31.0); // 50 * 0.6214, truncated
        assert_eq!(t.suspension_travel_lf, 10.0 / 1000.0);
        assert_eq!(t.suspension_travel_rr, 20.0 / 1000.0);
        assert_eq!(t.steering_wheel_angle, 5.0);
        assert_eq!(t.vertical_acceleration, 1.25);
        assert_eq!((t.lateral_acceleration, t.longitudinal_acceleration), (2.0, f64::from(0.3f32) * 10.0));
        assert_eq!(t.lateral_velocity, f64::from(0.2f32) * 10.0);
        assert_eq!((t.vertical_velocity, t.longitudinal_velocity), (-1.0, 3.0));
        assert!((t.roll - 30.0).abs() < 1e-9);
        assert_eq!(t.vehicle, "Renault");
    }

    #[test]
    fn extradata3_packet_falls_back_to_derived_values() {
        // 264 bytes do not hold the 304-byte layout -> the 256-byte layout is used; no gforce_vert / ang_vel.
        let (mut source, socket) = source();
        let mut packet = encode(
            Layout::Extradata3,
            &[
                (field::TIME, 2.0),
                (field::RX, 1.0),
                (field::FY, 1.0),
                (field::ZV, -4.0),
                (field::XV, 1.5),
                (field::TEAM_INFO, -1.0),
            ],
        );
        packet.extend_from_slice(&[0; 8]);
        let t = run(&mut source, &socket, &packet);
        assert_eq!(t.vertical_acceleration, 2.0); // (4 - 0) / (2 - 0), not doubled
        assert_eq!((t.lateral_velocity, t.vertical_velocity, t.longitudinal_velocity), (1.5, 0.0, 0.0));
        assert_eq!(t.vehicle, "Manor"); // era -1, last slot of the 2016 table
        assert_eq!(t.gear, -1.0); // 0.0 truncated, minus 1
    }

    #[test]
    fn short_datagram_keeps_the_previous_packet() {
        let (mut source, socket) = source();
        run(&mut source, &socket, &encode(Layout::Extradata3, &[(field::RPM, 900.0)]));
        socket.push(&[1u8; 100]);
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(!source.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 900.0);
    }

    #[test]
    fn poll_before_the_first_packet_stores_only_name_location_and_lateral_velocity() {
        // There is no packet at start and a datagram shorter than 256 bytes parses nothing: every
        // read fails except car_name (era -1 lookup -> 0) and the unmapped track_loc (0);
        // the dispatcher stores those and lateral_velocity 0, then its top-speed update fails on the
        // speed read and ends the pass before pitch, yaw, roll or the auto-loaded channels.
        let (mut source, socket) = source();
        let mut t = Telemetry::off();
        (t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag) = (11.0, 22.0, 33.0, 44.0, 77.0, 3.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        socket.push(&[1u8; 100]);
        assert!(source.is_running().unwrap());
        assert_ends(source.poll(&mut t));
        assert_eq!((t.pitch, t.yaw, t.roll, t.rpm, t.lateral_velocity, t.raw_flag), (11.0, 22.0, 33.0, 44.0, 0.0, 3.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("", ""));
    }

    #[test]
    fn nan_speed_stores_car_name_and_lateral_velocity_only() {
        // The NaN speed read fails in the top-speed update; car_name and lateral_velocity
        // (angular_velocity x * 10) were stored before it.
        let (mut source, socket) = source();
        socket.push(&encode(
            Layout::Extended,
            &[
                (field::SPEED, f32::NAN),
                (field::RPM, 900.0),
                (field::ANG_VEL_X, 0.5),
                (field::ERA, 2017.0),
                (field::TEAM_INFO, 3.0),
            ],
        ));
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        t.rpm = 44.0;
        assert_ends(source.poll(&mut t));
        assert_eq!((t.rpm, t.lateral_velocity, t.vehicle.as_str()), (44.0, 5.0, "Renault"));
    }

    #[test]
    fn car_name_tables_follow_the_era() {
        // car_name: era -1 lookups answer 0 on failure; era 2017 / 1980
        // lookups do not, so their failure fails the read.
        let named = |era: f32, team: f32| {
            let bytes = encode(Layout::Extended, &[(field::ERA, era), (field::TEAM_INFO, team)]);
            car_name(&MotionPacket::decode(&bytes, Layout::Extended).unwrap())
        };
        assert_eq!(named(-1.0, 12.0), CarName::Named("Manor"));
        assert_eq!(named(-1.0, 13.0), CarName::Blank);
        assert_eq!(named(-1.0, f32::NAN), CarName::Blank);
        assert_eq!(named(2017.0, 9.0), CarName::Blank);
        assert_eq!(named(2017.0, 40.0), CarName::Raised);
        assert_eq!(named(2017.0, f32::NAN), CarName::Raised);
        assert_eq!(named(1980.0, 11.0), CarName::Named("Redbull 2010"));
        assert_eq!(named(1980.0, 13.0), CarName::Raised);
        assert_eq!(named(2016.0, 0.0), CarName::Blank);
        // A NaN era has no integer value: era stays -1.
        assert_eq!(named(f32::NAN, 0.0), CarName::Named("Redbull"));
        assert_eq!(game_from_windows(&["GRID 2"]), Some("grid2"));
    }

    #[test]
    fn a_raising_car_name_lookup_keeps_the_previous_vehicle() {
        // The dispatcher keeps the previous vehicle name when the car_name read fails.
        for team in [40.0, f32::NAN] {
            let (mut source, socket) = source();
            socket.push(&encode(
                Layout::Extended,
                &[(field::RPM, 900.0), (field::ERA, 2017.0), (field::TEAM_INFO, team)],
            ));
            assert!(source.is_running().unwrap());
            let mut t = Telemetry::off();
            t.vehicle = "Earlier car".into();
            source.poll(&mut t).unwrap();
            assert_eq!((t.vehicle.as_str(), t.rpm), ("Earlier car", 900.0));
        }
    }

    #[test]
    fn gear_is_truncated_before_subtracting_one() {
        // gear: the packet's gear truncated, minus 1.
        let (mut source, socket) = source();
        let t = run(&mut source, &socket, &encode(Layout::Extended, &[(field::GEAR, 0.5)]));
        assert_eq!(t.gear, -1.0);
    }

    #[test]
    fn unmapped_channels_read_zero_on_every_tick() {
        // Unanswered names read 0, which the dispatcher stores for every
        // auto-loaded channel; track_loc is unmapped too.
        let (mut source, socket) = source();
        socket.push(&encode(Layout::Extended, &[(field::RPM, 900.0), (field::ERA, -1.0), (field::TEAM_INFO, 1.0)]));
        assert!(source.is_running().unwrap());
        let mut t = Telemetry::off();
        (t.wheel_slip_lf, t.raw_flag, t.fuel_level) = (0.4, 3.0, 12.0);
        t.location = "Monza".into();
        source.poll(&mut t).unwrap();
        assert_eq!((t.wheel_slip_lf, t.raw_flag, t.fuel_level), (0.0, 0.0, 0.0));
        assert_eq!((t.location.as_str(), t.vehicle.as_str()), ("", "Ferrari"));
    }

    #[test]
    fn receive_data_parses_the_datagram_check_status_received() {
        // The liveness test receives the datagram, the receive step parses it; the receive step
        // receives nothing itself.
        let (mut source, socket) = source();
        socket.push(&encode(Layout::Extended, &[(field::RPM, 100.0)]));
        assert!(source.is_running().unwrap());
        socket.push(&encode(Layout::Extended, &[(field::RPM, 200.0)]));
        let mut t = Telemetry::off();
        for _ in 0..2 {
            assert!(source.poll(&mut t).unwrap());
            assert_eq!(t.rpm, 100.0);
        }
        assert_eq!(socket.activity().reopened, 1);
        assert_eq!(socket.pending(), 1);
    }

    #[test]
    fn a_bind_failure_is_a_game_error_until_a_datagram_is_parsed() {
        // A failed liveness test adds its text to the game errors; the receive step clears them after a parse.
        let (mut source, socket) = source();
        socket.refuse_bind(true);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().len(), 1);
        socket.refuse_bind(false);
        run(&mut source, &socket, &[0u8; 8]);
        assert_eq!(source.game_errors().len(), 1, "a datagram neither layout takes clears nothing");
        run(&mut source, &socket, &[0u8; 256]);
        assert!(source.game_errors().is_empty());
    }
}

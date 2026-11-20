//! DiRT 2/3/4, DiRT Rally (2.0), DiRT Showdown, F1 2011/2012 (listener `dirt`).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::kinematics::{
    frame_pitch, frame_yaw, lateral_speed, roll_degrees, vertical_speed, HeaveEstimator, HeaveRule,
};
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
/// `max_rpm` when the packet has no `max_rpm` field.
const FALLBACK_MAX_RPM: f64 = 20000.0;

/// Which DiRT game an open window title names, checked in this order: `"DiRT 3"`, `"DiRT 4"`,
/// `"DiRT Rally"`, `"DiRT Rally 2.0"` (a DiRT Rally 2.0 window therefore answers `"dirtrally"`).
/// This is the `game` channel.
pub fn game_from_windows<S: AsRef<str>>(windows: &[S]) -> Option<&'static str> {
    [("DiRT 3", "dirt3"), ("DiRT 4", "dirt4"), ("DiRT Rally", "dirtrally"), ("DiRT Rally 2.0", "dirtrally2.0")]
        .into_iter()
        .find(|(needle, _)| windows.iter().any(|w| w.as_ref().contains(needle)))
        .map(|(_, game)| game)
}

/// The DiRT listener: synchronous, one datagram per `is_running` probe and one per `poll`.
pub struct DirtSource {
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

impl DirtSource {
    /// Listen on `127.0.0.1:<settings.port>`.
    pub fn new(settings: &ListenerSettings) -> Self {
        let socket = UdpEndpoint::new(Endpoint::loopback(settings.port, TIMEOUT));
        DirtSource::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
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
        DirtSource {
            socket,
            native_socket: false,
            socket_assigned: false,
            forwarder,
            connected: false,
            packet: None,
            game_errors: GameErrors::default(),
            heave: Mutex::new(HeaveEstimator::new(HeaveRule::HoldLastNonZero)),
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

    /// The receive step: one datagram; a 152-byte extradata=1 packet is parsed but marks
    /// the game as not connected; anything else is parsed as an extradata=3 packet from its first 256 bytes.
    fn receive(&mut self) -> bool {
        let datagram = match self.socket.receive(RECEIVE_SIZE) {
            Ok(datagram) => datagram,
            Err(_) => {
                self.disconnect();
                return false;
            }
        };
        self.forwarder.forward(&datagram);
        if datagram.len() == Layout::Extradata1.bytes() {
            self.packet = MotionPacket::decode(&datagram, Layout::Extradata1);
            self.connected = false;
            self.game_errors.clear();
            return true;
        }
        match MotionPacket::decode_prefix(&datagram, Layout::Extradata3) {
            Some(packet) => {
                self.packet = Some(packet);
                self.connected = true;
                self.game_errors.clear();
                true
            }
            None => {
                self.disconnect();
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
        let lateral_velocity = lateral_speed(&p.frame);
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
        t.suspension_travel_lf = p.suspension_position.front_left / 10000.0;
        t.suspension_travel_lr = p.suspension_position.rear_left / 10000.0;
        t.suspension_travel_rf = p.suspension_position.front_right / 10000.0;
        t.suspension_travel_rr = p.suspension_position.rear_right / 10000.0;
        t.suspension_velocity_lf = p.suspension_velocity.front_left;
        t.suspension_velocity_lr = p.suspension_velocity.rear_left;
        t.suspension_velocity_rf = p.suspension_velocity.front_right;
        t.suspension_velocity_rr = p.suspension_velocity.rear_right;
        t.steering_wheel_angle = p.steer;
        t.longitudinal_acceleration = p.gforce_lon * 10.0;
        t.lateral_acceleration = p.gforce_lat * 10.0;
        // A failed roll computation makes the dispatcher store 0 for roll.
        t.roll = roll_degrees(p.frame.right[1]).map_or(0.0, |roll| -roll);
        t.pitch = frame_pitch(&p.frame);
        t.yaw = frame_yaw(&p.frame);
        t.lateral_velocity = lateral_velocity;
        t.vertical_acceleration =
            self.heave.get_mut().unwrap_or_else(PoisonError::into_inner).next(p.time, vertical_speed(&p.frame)) * 2.0;
        Ok(())
    }

    /// The liveness test: while not connected, rebind and wait up to 0.2 s for a datagram.
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

impl Source for DirtSource {
    fn id(&self) -> &'static str {
        "dirt"
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        self.check_status()
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }
    fn read_native(&self, name: &str) -> NativeRead {
        super::native::read(self.packet, &self.heave, super::native::Kind::Dirt, name)
    }
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            _ => None,
        }
    }

    /// The receive step, then the channel reads; channels are published every tick, as the dispatcher reads them
    /// whatever the receive step did.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let received = self.receive();
        self.publish(telemetry)?;
        Ok(received)
    }
}

#[cfg(test)]
pub(super) fn assert_native_creation_ends_dispatcher(source: impl Source + 'static) {
    use crate::derive::{
        AttitudeSource, DamsMode, DamsSettings, DispatchSettings, Dispatcher, GameDirectory, IntellituneReference,
        ProfileSettings, RefreshRates, RuntimeInputs, ScanHit, SourceListener, SpeedUnit, TickInput,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Games;
    impl GameDirectory for Games {
        fn is_flying(&self, _: &str) -> bool {
            false
        }
        fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
            None
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
            panic!("later source must not be polled")
        }
    }
    let id = source.id();
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
        ScanHit { game_id: id.into(), listener_id: id.into() },
        ScanHit { game_id: "later".into(), listener_id: "later".into() },
    ];
    let input = TickInput { hits: &hits, settings: &settings, profile: &profile, runtime: &runtime, games: &Games };
    let outcome = dispatcher.tick(&input);
    assert!(dispatcher.has_ended());
    assert_eq!(outcome.events, [crate::derive::DispatchEvent::GameRunning(id.into())]);
    assert_eq!(outcome.sleep, Duration::ZERO);
    assert!(dispatcher.telemetry_writes().is_empty());
    assert_eq!(later.load(Ordering::Relaxed), 0);
    dispatcher.tick(&input);
    assert_eq!(later.load(Ordering::Relaxed), 0);
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
            DirtSource::with_native_socket(Box::new(Failure(attempts.clone())), Arc::new(Forwarder::default()));
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
            DirtSource::with_socket(Box::new(Failure(compatibility_attempts.clone())), Arc::new(Forwarder::default()));
        assert!(
            !compatibility.is_running().unwrap(),
            "the public numeric fake constructor keeps its existing behavior"
        );
        assert_eq!(compatibility_attempts.load(Ordering::Relaxed), 101);
        assert!(compatibility.game_errors().contains("creation denied"));
        super::assert_native_creation_ends_dispatcher(DirtSource::with_native_socket(
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
        let data = encode(Layout::Extradata3, &[(field::SPEED, 25.0), (field::RPM, 512.5)]);
        socket.push(&data);
        let attempts = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let mut source = DirtSource::with_native_socket(
            Box::new(Staged { attempts: attempts.clone(), closes: closes.clone(), socket }),
            Arc::new(Forwarder::default()),
        );
        source.packet = MotionPacket::decode(&data, Layout::Extradata3);
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
    fn native_receive_does_not_replay_getters() {
        let (mut source, _) = source();
        source.packet = MotionPacket::decode(
            &encode(
                Layout::Extradata3,
                &[
                    (field::TIME, 0.5),
                    (field::RPM, 500.0),
                    (field::SPEED, 30.0),
                    (field::RX, 1.0),
                    (field::FY, 1.0),
                    (field::ZV, -1.0),
                ],
            ),
            Layout::Extradata3,
        );
        let mut t = Telemetry { speed: 999.0, ..Telemetry::off() };
        source.receive_native(&mut t).unwrap();
        assert_eq!(t.speed, 999.0);
    }
    use crate::udp_racing::codemasters::packet::{encode, field};
    use crate::udp_racing::listener_output::zero_dispatcher_channels;
    use crate::udp_racing::socket::scripted::ScriptedSocket;
    use crate::udp_racing::unhandled::assert_ends;

    fn source() -> (DirtSource, ScriptedSocket) {
        let socket = ScriptedSocket::new();
        (DirtSource::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default())), socket)
    }

    /// A moving car: right = +x, forward = +y (pitch 90, yaw 90), up = -z, vertical speed 2 m/s.
    fn moving(layout: Layout, extra: &[(usize, f32)]) -> Vec<u8> {
        let mut fields = vec![
            (field::TIME, 1.0),
            (field::SPEED, 25.7),
            (field::RPM, 512.5),
            (field::GEAR, 3.0),
            (field::RX, 1.0),
            (field::FY, 1.0),
            (field::ZV, -2.0),
            (field::XV, 0.5),
        ];
        fields.extend_from_slice(extra);
        encode(layout, &fields)
    }

    fn connect(source: &mut DirtSource, socket: &ScriptedSocket) {
        socket.push(b"probe");
        assert!(source.is_running().unwrap());
    }

    #[test]
    fn check_status_rebinds_and_needs_one_datagram() {
        // Liveness: create, bind, set the timeout, receive -> connected.
        let (mut source, socket) = source();
        assert!(!source.is_running().unwrap());
        assert_eq!(socket.activity().reopened, 1);
        connect(&mut source, &socket);
        assert_eq!(socket.activity().reopened, 2);
        // Connected: no socket work at all.
        assert!(source.is_running().unwrap());
        assert_eq!(socket.activity().reopened, 2);
        assert_eq!(socket.activity().receives, vec![512, 512]);
    }

    #[test]
    fn check_status_fails_when_bind_fails() {
        let (mut source, socket) = source();
        socket.refuse_bind(true);
        socket.push(b"never read");
        assert!(!source.is_running().unwrap());
        assert_eq!(socket.pending(), 1);
    }

    #[test]
    fn receive_data_maps_extradata3_channels() {
        // Channel values.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        let mut packet = moving(
            Layout::Extradata3,
            &[
                (field::MAX_RPM, 750.0),
                (field::SUSP_POS_FL, 100.0),
                (field::SUSP_POS_RL, 200.0),
                (field::SUSP_POS_FR, 300.0),
                (field::SUSP_POS_RR, 400.0),
                (field::SUSP_VEL_FL, 1.5),
                (field::STEER, -0.25),
                (field::GFORCE_LON, 0.5),
                (field::GFORCE_LAT, -0.25),
            ],
        );
        packet.extend_from_slice(&[0; 8]); // DiRT Rally sends 264 bytes; only the first 256 are parsed.
        socket.push(&packet);
        let mut t = Telemetry::off();
        assert!(source.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 5125.0);
        assert_eq!(t.max_rpm, 7500.0);
        assert_eq!(t.gear, 3.0);
        assert_eq!(t.speed, 25.0);
        assert_eq!(t.suspension_travel_lf, 100.0 / 10000.0);
        assert_eq!(t.suspension_travel_lr, 200.0 / 10000.0);
        assert_eq!(t.suspension_travel_rf, 300.0 / 10000.0);
        assert_eq!(t.suspension_travel_rr, 400.0 / 10000.0);
        assert_eq!(t.suspension_velocity_lf, 1.5);
        assert_eq!(t.steering_wheel_angle, -0.25);
        assert_eq!(t.longitudinal_acceleration, 5.0);
        assert_eq!(t.lateral_acceleration, -2.5);
        assert_eq!((t.pitch, t.yaw, t.lateral_velocity), (90.0, 90.0, 0.5));
        assert_eq!(t.roll, -0.0);
        // First frame: (2 - 0) / (1 - 0) * 2.
        assert_eq!(t.vertical_acceleration, 4.0);
        assert!(source.is_running().unwrap());
    }

    #[test]
    fn extradata1_packet_is_parsed_but_disconnects() {
        // A 152-byte datagram is an extradata=1 packet and marks not connected. max_rpm falls back to 20000.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&moving(Layout::Extradata1, &[]));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!((t.rpm, t.max_rpm), (5125.0, 20000.0));
        assert!(socket.is_open());
        assert_eq!(socket.activity().closed, 0);
        socket.push_timeout();
        assert!(!source.is_running().unwrap());
    }

    #[test]
    fn short_or_missing_datagram_disconnects_and_keeps_the_last_packet() {
        // A failed receive closes the socket and marks not connected. Channels still answer from the old packet.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&moving(Layout::Extradata3, &[]));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        socket.push(&[0u8; 200]);
        t.rpm = -1.0;
        assert!(!source.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 5125.0);
        assert!(!socket.is_open());
        socket.push_timeout();
        assert!(!source.is_running().unwrap());
    }

    #[test]
    fn stationary_menu_packet_reads_zero_everywhere() {
        // Stationary test: rpm*10 and speed truncate to 0 and pitch is 90 -> every channel reads 0.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&encode(Layout::Extradata3, &[(field::RPM, 0.09), (field::SPEED, 0.9), (field::YV, 5.0)]));
        let mut t = Telemetry::off();
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        source.poll(&mut t).unwrap();
        assert_eq!((t.gear, t.max_rpm, t.vertical_acceleration, t.tyre_wear_lf), (0.0, 0.0, 0.0, 0.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("", ""));
        // The guard skipped the heave estimator: the next moving frame still differentiates from time 0.
        socket.push(&moving(Layout::Extradata3, &[]));
        source.poll(&mut t).unwrap();
        assert_eq!(t.vertical_acceleration, 4.0);
    }

    #[test]
    fn rpm_threshold_of_the_guard_is_int_truncation() {
        // 0.1 * 10 truncates to 1 -> not stationary.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&encode(Layout::Extradata3, &[(field::RPM, 0.1)]));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 1.0);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.pitch, 90.0);
    }

    #[test]
    fn roll_is_zero_when_asin_raises() {
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&moving(Layout::Extradata3, &[(field::RY, 1.5)]));
        let mut t = Telemetry::off();
        t.roll = 7.0;
        source.poll(&mut t).unwrap();
        assert_eq!(t.roll, 0.0);
    }

    /// Channels left over from an earlier tick (say an F1 2019 session), to see which ones a tick overwrites.
    fn stale() -> Telemetry {
        let mut t = Telemetry::off();
        (t.pitch, t.yaw, t.roll, t.rpm, t.speed, t.gear, t.lateral_velocity) =
            (11.0, 22.0, 33.0, 44.0, 55.0, 6.0, 77.0);
        (t.raw_flag, t.wheel_slip_lf, t.longitudinal_velocity) = (3.0, 0.4, 8.0);
        (t.vehicle, t.location) = ("Earlier car".into(), "Monza".into());
        t
    }

    /// The speed read failed: car name per `vehicle`, location 0 and lateral_velocity were stored, nothing else.
    fn assert_speed_unreadable(t: &Telemetry, lateral_velocity: f64, vehicle: &str) {
        assert_eq!((t.pitch, t.yaw, t.roll, t.rpm, t.speed, t.gear), (11.0, 22.0, 33.0, 44.0, 55.0, 6.0));
        assert_eq!((t.raw_flag, t.wheel_slip_lf, t.longitudinal_velocity), (3.0, 0.4, 8.0));
        assert_eq!(t.lateral_velocity, lateral_velocity);
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), (vehicle, ""));
    }

    #[test]
    fn nan_rpm_stores_only_location_and_a_zero_lateral_velocity() {
        // A NaN rpm fails the stationary test for every channel. The dispatcher keeps
        // the vehicle name, stores location 0 and lateral_velocity 0, then
        // its top-speed update fails on the speed read and ends the pass before pitch, yaw, roll or any
        // auto-loaded channel is read.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&moving(Layout::Extradata3, &[(field::RPM, f32::NAN)]));
        let mut t = stale();
        assert_ends(source.poll(&mut t));
        assert_speed_unreadable(&t, 0.0, "Earlier car");
    }

    #[test]
    fn nan_speed_of_a_running_engine_stores_only_name_location_and_lateral_velocity() {
        // The stationary test stops at a non-zero rpm, so car_name and track_loc read 0;
        // the NaN speed read fails in the top-speed update, after the dispatcher stored the (readable)
        // lateral_velocity rx*xv + ry*yv + rz*zv.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&moving(Layout::Extradata3, &[(field::SPEED, f32::NAN)]));
        let mut t = stale();
        assert_ends(source.poll(&mut t));
        assert_speed_unreadable(&t, 0.5, "");
    }

    #[test]
    fn poll_before_the_first_packet_reads_zero_everywhere() {
        // No packet yet: every channel reads 0.
        // Nothing fails, so the dispatcher stores 0 for speed, pitch, yaw, roll, lateral_velocity, every
        // auto-loaded channel, the car name and the location. The receive here times out: no packet parsed.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        let mut t = stale();
        assert!(!source.poll(&mut t).unwrap());
        let mut zero = Telemetry::off();
        zero_dispatcher_channels(&mut zero);
        assert_eq!(t, zero);
    }

    #[test]
    fn unmapped_channels_read_zero_on_every_tick() {
        // Every name the listener does not answer reads 0, and
        // the dispatcher stores it for every auto-loaded channel, car_name and track_loc.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&moving(Layout::Extradata3, &[]));
        let mut t = stale();
        source.poll(&mut t).unwrap();
        assert_eq!((t.raw_flag, t.wheel_slip_lf, t.longitudinal_velocity, t.fuel_level), (0.0, 0.0, 0.0, 0.0));
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("", ""));
        assert_eq!(t.rpm, 5125.0);
    }

    #[test]
    fn a_nan_gear_keeps_the_previous_gear() {
        // A NaN gear has no integer value; the channel has no default, so the dispatcher keeps it.
        let (mut source, socket) = source();
        connect(&mut source, &socket);
        socket.push(&moving(Layout::Extradata3, &[(field::GEAR, f32::NAN)]));
        let mut t = stale();
        source.poll(&mut t).unwrap();
        assert_eq!((t.gear, t.raw_flag), (6.0, 0.0));
    }

    #[test]
    fn game_names_follow_the_original_check_order() {
        assert_eq!(game_from_windows(&["DiRT Rally 2.0"]), Some("dirtrally"));
        assert_eq!(game_from_windows(&["Steam", "DiRT 4"]), Some("dirt4"));
        assert_eq!(game_from_windows(&["DiRT Showdown"]), None);
    }

    #[test]
    fn a_bind_failure_is_a_game_error_until_a_datagram_arrives() {
        // A failed bind or timeout setting adds its text to the game errors; the receive step
        // clears them after either length branch.
        let (mut source, socket) = source();
        socket.refuse_bind(true);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().len(), 1);
        socket.refuse_bind(false);
        connect(&mut source, &socket);
        assert_eq!(source.game_errors().len(), 1, "checkStatus alone does not clear");
        socket.push(&[0u8; 152]);
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert!(source.game_errors().is_empty());
    }
}

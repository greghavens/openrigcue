//! EA SPORTS WRC (listener `wrc2023`): the `wrc_srs` custom UDP structure the game-side patch installs.

use std::collections::BTreeSet;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::parameters::GameErrors;
pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::{rad_to_deg, trunc};
use super::support::receiver::{ReceiveFailure, Receiver};
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{NativeRead, Source, SourceError};

/// `NETWORK_PORT.wrc2023` as seeded by configuration migration 334.
/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 20999;
const RECEIVE_SIZE: usize = 512;
const RECEIVE_TIMEOUT: Duration = Duration::from_millis(100);
/// Size of the `session_update` packet described by `wrc_srs.json`.
const PACKET_LEN: usize = 200;
/// Quarter turn (`1.5708`, not `FRAC_PI_2`).
#[allow(clippy::approx_constant)]
const QUARTER_TURN: f64 = 1.5708;

/// The packet fields the listener uses (byte-packed).
#[derive(Debug, Clone, Copy, PartialEq)]
struct EaApi {
    gear_index: u8,
    speed: f64,
    velocity: [f64; 3],
    acceleration: [f64; 3],
    left_direction_y: f64,
    forward_direction: [f64; 3],
    /// Back-left, back-right, front-left, front-right.
    hub_position: [f64; 4],
    rpm_max: f64,
    rpm_current: f64,
    steering: f64,
}

impl EaApi {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        let v3 = |o: usize| -> Result<[f64; 3], Short> { Ok([f.f32(o)?, f.f32(o + 4)?, f.f32(o + 8)?]) };
        Ok(EaApi {
            gear_index: f.u8(0)?,
            speed: f.f32(4)?,
            velocity: v3(24)?,
            acceleration: v3(36)?,
            left_direction_y: f.f32(52)?,
            forward_direction: v3(60)?,
            hub_position: [f.f32(84)?, f.f32(88)?, f.f32(92)?, f.f32(96)?],
            rpm_max: f.f32(148)?,
            rpm_current: f.f32(156)?,
            steering: f.f32(172)?,
        })
    }
}

/// The arcsine, which fails outside [-1, 1].
fn asin(x: f64) -> Option<f64> {
    (x.abs() <= 1.0 || x.is_nan()).then(|| x.asin())
}

/// The EA SPORTS WRC listener.
pub struct Wrc2023Source {
    receiver: Receiver,
    telemetry: Option<EaApi>,
    game_errors: GameErrors,
    native_socket: bool,
    socket_assigned: bool,
}

impl Wrc2023Source {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    /// Cache `NETWORK_PORT.wrc2023` as an integer, leaving the range check to the bind.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(0, RECEIVE_TIMEOUT);
        Self {
            receiver: Receiver::with_imported_port(net, endpoint, RECEIVE_SIZE, port),
            telemetry: None,
            game_errors: GameErrors::default(),
            native_socket: true,
            socket_assigned: false,
        }
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, RECEIVE_TIMEOUT);
        let receiver = Receiver::new(net, endpoint, RECEIVE_SIZE);
        Wrc2023Source {
            receiver,
            telemetry: None,
            game_errors: GameErrors::default(),
            native_socket: false,
            socket_assigned: false,
        }
    }

    /// The error texts this listener contributes to the game errors.
    pub fn game_errors(&self) -> &BTreeSet<String> {
        self.game_errors.messages()
    }

    /// Writes the channels. Until a 200-byte packet was decoded there is no telemetry and every channel
    /// reads 0. The standstill guard (integer rpm 0 and integer speed 0) evaluates the speed only when the
    /// rpm truncates to 0; where a value of the guard has no integer value, every read fails, `car_name` and
    /// `lateral_velocity` included, and where the speed read's own conversion fails the speed alone does: either
    /// way the tick ends after `lateral_velocity` ([`publish_speed_unreadable`]). Otherwise a read that fails
    /// gets the dispatcher's value for it: the previous max rpm or vertical acceleration (registry default
    /// `None`), 0 for roll and pitch; the channel read handles no failure itself.
    fn write_channels(&self, out: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let Some(t) = self.telemetry else {
            zero_listener_channels(out);
            return Ok(());
        };
        let Some(rpm) = trunc(t.rpm_current) else { return publish_speed_unreadable(out, None, 0.0) };
        let speed = trunc(t.speed);
        if rpm == 0.0 {
            match speed {
                None => return publish_speed_unreadable(out, None, 0.0),
                Some(0.0) => {
                    zero_listener_channels(out);
                    return Ok(());
                }
                Some(_) => {}
            }
        }
        let heading = t.forward_direction[0].atan2(t.forward_direction[2]);
        let side = heading + QUARTER_TURN;
        let [vx, _, vz] = t.velocity;
        let lateral_velocity = -(side.sin() * vx + side.cos() * vz);
        let Some(speed) = speed else {
            return publish_speed_unreadable(out, Some(String::new()), lateral_velocity);
        };
        let (previous_max_rpm, previous_vertical) = (out.max_rpm, out.vertical_acceleration);
        zero_listener_channels(out);
        let [ax, ay, az] = t.acceleration;
        out.rpm = rpm;
        out.max_rpm = trunc(t.rpm_max).unwrap_or(previous_max_rpm);
        out.gear = f64::from(t.gear_index);
        out.speed = speed;
        out.steering_wheel_angle = t.steering;
        out.longitudinal_acceleration = heading.sin() * ax + heading.cos() * az;
        out.lateral_acceleration = -(side.sin() * ax + side.cos() * az);
        match asin(t.left_direction_y) {
            Some(r) => {
                out.roll = rad_to_deg(r);
                out.vertical_acceleration = r.cos() * ay;
            }
            None => out.vertical_acceleration = previous_vertical,
        }
        if let Some(p) = asin(t.forward_direction[1]) {
            out.pitch = rad_to_deg(p);
        }
        out.yaw = rad_to_deg(heading);
        out.lateral_velocity = lateral_velocity;
        let [bl, br, fl, fr] = t.hub_position;
        (out.suspension_travel_lf, out.suspension_travel_lr, out.suspension_travel_rf, out.suspension_travel_rr) =
            (fl, bl, fr, br);
        Ok(())
    }
}

impl Source for Wrc2023Source {
    fn id(&self) -> &'static str {
        "wrc2023"
    }

    fn read_native(&self, name: &str) -> NativeRead {
        use LegacyValue::{Float, Int};
        let Some(t) = self.telemetry else { return NativeRead::Value(Int(0)) };
        let Some(rpm) = trunc(t.rpm_current) else { return NativeRead::Raised };
        if rpm == 0.0 {
            match trunc(t.speed) {
                None => return NativeRead::Raised,
                Some(0.0) => return NativeRead::Value(Int(0)),
                Some(_) => {}
            }
        }
        let heading = t.forward_direction[0].atan2(t.forward_direction[2]);
        let side = heading + QUARTER_TURN;
        let [ax, ay, az] = t.acceleration;
        let [vx, _, vz] = t.velocity;
        let value = match name {
            "rpm" => super::native_int(t.rpm_current),
            "max_rpm" => super::native_int(t.rpm_max),
            "speed" => super::native_int(t.speed),
            "gear" => Some(Int(i64::from(t.gear_index))),
            "roll" => asin(t.left_direction_y).map(|r| Float(rad_to_deg(r))),
            "pitch" => asin(t.forward_direction[1]).map(|p| Float(rad_to_deg(p))),
            "yaw" => Some(Float(rad_to_deg(heading))),
            "steering_wheel_angle" => Some(Float(t.steering)),
            "longitudinal_acceleration" => Some(Float(heading.sin() * ax + heading.cos() * az)),
            "lateral_acceleration" => Some(Float(-(side.sin() * ax + side.cos() * az))),
            "vertical_acceleration" => asin(t.left_direction_y).map(|r| Float(r.cos() * ay)),
            "lateral_velocity" => Some(Float(-(side.sin() * vx + side.cos() * vz))),
            "suspension_travel_lf" => Some(Float(t.hub_position[2])),
            "suspension_travel_lr" => Some(Float(t.hub_position[0])),
            "suspension_travel_rf" => Some(Float(t.hub_position[3])),
            "suspension_travel_rr" => Some(Float(t.hub_position[1])),
            _ => Some(Int(0)),
        };
        value.map_or(NativeRead::Raised, NativeRead::Value)
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    /// The liveness test: any datagram; the socket stays open for the receive step. A bind failure is
    /// recorded in the game errors, a receive failure only logged.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_and_keep() {
            Ok(_) => {
                self.socket_assigned = true;
                true
            }
            Err(ReceiveFailure::Bind(e)) => {
                if self.native_socket && socket_creation_failed(&e) {
                    if !self.socket_assigned {
                        return Err(
                            DispatcherEnds::new("AttributeError: 'NoneType' object has no attribute 'close'").into()
                        );
                    }
                    self.receiver.close();
                    return Ok(false);
                }
                self.socket_assigned = true;
                self.game_errors.record(&e);
                false
            }
            Err(ReceiveFailure::Receive(e)) => {
                self.socket_assigned = true;
                tracing::debug!("Error {e}");
                false
            }
        };
        Ok(running)
    }

    /// The receive step: a second datagram, decoded when it has the 200-byte packet size; any received
    /// datagram clears the game errors.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = match self.receiver.receive_more() {
            Ok(data) => {
                self.game_errors.clear();
                let decoded = data.len() == PACKET_LEN;
                if decoded {
                    self.telemetry = EaApi::parse(&data).ok();
                }
                decoded
            }
            Err(e) => {
                tracing::debug!("Error {e}");
                false
            }
        };
        Ok(fresh)
    }

    /// Numeric compatibility receive/getter projection. Typed dispatch evaluates each getter at its stage.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry)?;
        Ok(fresh)
    }

    fn stop(&mut self) {
        self.receiver.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;
    use crate::udp_racing::unhandled::assert_ends;

    #[test]
    fn first_creation_failure_preserves_native_none_close_error() {
        struct Fails;
        impl Network for Fails {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn super::super::support::net::Datagrams>> {
                Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("create failed")))
            }
        }
        let mut source = Wrc2023Source::with_imported_network(ImportedPort::Valid(DEFAULT_PORT), Box::new(Fails));
        let error = source.is_running().expect_err("initial sock is None, so the creation catch raises");
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error));
        assert!(error.to_string().contains("AttributeError: 'NoneType' object has no attribute 'close'"));
        assert!(source.game_errors().is_empty());
    }

    #[test]
    fn native_socket_retry_preserves_telemetry_and_only_records_late_setup_errors() {
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
                    1 => Err(io::Error::other("bind failed")),
                    2 => Err(crate::udp_racing::socket::socket_creation_error(io::Error::other("create failed"))),
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
        ready.push(6001, &[1]);
        ready.push(6001, &packet(4000.0, 20.0));
        let mut source =
            Wrc2023Source::with_imported_network(ImportedPort::Valid(6001), Box::new(Staged(calls.clone(), ready)));
        source.telemetry = Some(EaApi::parse(&packet(3000.0, 10.0)).unwrap());
        for count in [1, 1, 2] {
            assert!(!source.is_running().unwrap());
            assert!(source.socket_assigned);
            assert_eq!(source.game_errors().len(), count);
            assert_eq!(source.telemetry.as_ref().unwrap().rpm_current, 3000.0);
        }
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert!(source.game_errors().is_empty());
        assert_eq!(source.telemetry.as_ref().unwrap().rpm_current, 4000.0);
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 4]);
    }

    #[test]
    fn native_first_creation_error_stops_dispatcher_before_the_next_listener() {
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
        struct Fails;
        impl Network for Fails {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn super::super::support::net::Datagrams>> {
                Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("create failed")))
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
        let source = Wrc2023Source::with_imported_network(ImportedPort::Valid(6001), Box::new(Fails));
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
            ScanHit { game_id: "wrc2023".into(), listener_id: "wrc2023".into() },
            ScanHit { game_id: "later".into(), listener_id: "later".into() },
        ];
        let input = TickInput { hits: &hits, settings: &settings, profile: &profile, runtime: &runtime, games: &Games };
        let outcome = dispatcher.tick(&input);
        assert!(dispatcher.has_ended());
        assert_eq!(later.load(Ordering::Relaxed), 0);
        assert_eq!(outcome.events, [crate::derive::DispatchEvent::GameRunning("wrc2023".into())]);
        assert!(dispatcher.telemetry_writes().is_empty());
        assert_eq!(outcome.sleep, Duration::ZERO);
        dispatcher.tick(&input);
        assert_eq!(later.load(Ordering::Relaxed), 0);
    }

    /// A `session_update` packet per `wrc_srs.json` (200 bytes).
    fn packet(rpm: f32, speed: f32) -> Vec<u8> {
        let mut d = vec![0u8; PACKET_LEN];
        let mut put = |o: usize, v: f32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(4, speed);
        put(24, 3.0);
        put(32, 4.0);
        put(36, 1.0);
        put(40, 9.81);
        put(44, 2.0);
        put(52, 0.5);
        put(60, 0.0);
        put(64, 0.0);
        put(68, 1.0);
        put(84, 0.1);
        put(88, 0.2);
        put(92, 0.3);
        put(96, 0.4);
        put(148, 8000.0);
        put(156, rpm);
        put(172, -0.25);
        d[0] = 3;
        d
    }

    fn polled(datagram: &[u8]) -> (bool, Telemetry) {
        let net = FakeNetwork::default();
        let mut s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, b"first datagram only proves liveness");
        net.push(DEFAULT_PORT, datagram);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        let fresh = s.poll(&mut t).unwrap();
        (fresh, t)
    }

    #[test]
    fn native_receive_does_not_predict_a_later_getter_failure() {
        let net = FakeNetwork::default();
        let mut s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &packet(f32::NAN, 20.0));
        assert!(s.is_running().unwrap());
        assert!(s.receive_native(&mut Telemetry::off()).unwrap(), "receiveData only decodes the packet");
    }

    fn decoded_source(data: &[u8]) -> Wrc2023Source {
        let mut s =
            Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        s.telemetry = Some(EaApi::parse(data).unwrap());
        s
    }

    #[test]
    fn native_getter_integer_float_gate_and_default_kinds() {
        let s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        for name in ["car_name", "rpm", "max_rpm", "speed", "roll"] {
            assert_eq!(s.read_native(name), NativeRead::Value(LegacyValue::Int(0)));
        }
        let s = decoded_source(&packet(4000.5, 20.7));
        for (name, value) in [
            ("rpm", LegacyValue::Int(4000)),
            ("max_rpm", LegacyValue::Int(8000)),
            ("gear", LegacyValue::Int(3)),
            ("speed", LegacyValue::Int(20)),
            ("steering_wheel_angle", LegacyValue::Float(-0.25)),
            ("longitudinal_acceleration", LegacyValue::Float(2.0)),
            ("suspension_travel_lf", LegacyValue::Float(f64::from(0.3f32))),
            ("yaw", LegacyValue::Float(0.0)),
            ("car_name", LegacyValue::Int(0)),
            ("missing", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.read_native(name), NativeRead::Value(value), "{name}");
        }
        let s = decoded_source(&packet(0.5, 0.9));
        for name in ["max_rpm", "gear", "steering_wheel_angle", "suspension_travel_lf", "missing"] {
            assert_eq!(s.read_native(name), NativeRead::Value(LegacyValue::Int(0)), "{name}");
        }
    }

    #[test]
    fn native_global_guard_and_individual_conversion_failures_remain_distinct() {
        for (rpm, speed) in [(f32::NAN, 20.0), (0.4, f32::NAN)] {
            let s = decoded_source(&packet(rpm, speed));
            for name in ["car_name", "track_loc", "speed", "rpm", "max_rpm", "roll", "missing"] {
                assert_eq!(s.read_native(name), NativeRead::Raised, "{name}");
            }
        }
        let mut s = decoded_source(&packet(4000.0, f32::NAN));
        assert_eq!(s.read_native("speed"), NativeRead::Raised);
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(4000)));
        assert_eq!(s.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
        s.telemetry.as_mut().unwrap().rpm_max = f64::INFINITY;
        assert_eq!(s.read_native("max_rpm"), NativeRead::Raised);
        assert!(matches!(s.read_native("lateral_velocity"), NativeRead::Value(LegacyValue::Float(_))));
    }

    #[test]
    fn native_asin_domain_errors_affect_only_the_requested_branches_and_nan_is_allowed() {
        let mut s = decoded_source(&packet(4000.0, 20.0));
        s.telemetry.as_mut().unwrap().left_direction_y = 1.5;
        assert_eq!(s.read_native("roll"), NativeRead::Raised);
        assert_eq!(s.read_native("vertical_acceleration"), NativeRead::Raised);
        assert_eq!(s.read_native("pitch"), NativeRead::Value(LegacyValue::Float(0.0)));
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(4000)));
        s.telemetry.as_mut().unwrap().left_direction_y = f64::NAN;
        for name in ["roll", "vertical_acceleration"] {
            assert!(matches!(s.read_native(name), NativeRead::Value(LegacyValue::Float(n)) if n.is_nan()));
        }
    }

    #[test]
    fn source_adapter_keeps_native_getter_failures_out_of_receive_data() {
        use crate::derive::{Listener, SourceListener};
        for (rpm, car) in [(f32::NAN, NativeRead::Raised), (4000.0, NativeRead::Value(LegacyValue::Int(0)))] {
            let net = FakeNetwork::default();
            let s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
            net.push(DEFAULT_PORT, &[1]);
            net.push(DEFAULT_PORT, &packet(rpm, f32::NAN));
            let mut listener = SourceListener::new(s);
            assert!(listener.is_running());
            listener.receive();
            assert_eq!(listener.failure_point(), None);
            assert_eq!(listener.read_native("car_name"), car);
            assert_eq!(listener.read_native("speed"), NativeRead::Raised);
        }
    }

    #[test]
    fn heading_based_channels() {
        // Channels with the car facing +z (heading 0)
        let (fresh, t) = polled(&packet(4000.5, 20.7));
        assert!(fresh);
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed, t.steering_wheel_angle), (4000.0, 8000.0, 3.0, 20.0, -0.25));
        assert_eq!(t.yaw, 0.0);
        assert_eq!(t.longitudinal_acceleration, 2.0);
        let side = QUARTER_TURN;
        assert_eq!(t.lateral_acceleration, -(side.sin() * 1.0 + side.cos() * 2.0));
        assert!((t.roll - 30.0).abs() < 1e-9);
        assert_eq!(t.vertical_acceleration, 0.5f64.asin().cos() * f64::from(9.81f32));
        assert_eq!(t.lateral_velocity, -(side.sin() * 3.0 + side.cos() * 4.0));
        assert_eq!((t.suspension_travel_lf, t.suspension_travel_lr), (f64::from(0.3f32), f64::from(0.1f32)));
    }

    #[test]
    fn engine_off_and_stopped_reads_zero() {
        // Standstill guard: integer rpm 0 and integer speed 0 -> 0
        let (_, t) = polled(&packet(0.5, 0.9));
        assert_eq!((t.max_rpm, t.gear, t.steering_wheel_angle), (0.0, 0.0, 0.0));
    }

    /// Poll one packet into a copy of `before`.
    fn polled_over(before: &Telemetry, datagram: &[u8]) -> Telemetry {
        let net = FakeNetwork::default();
        let mut s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, datagram);
        assert!(s.is_running().unwrap());
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        t
    }

    #[test]
    fn guard_int_failures_end_the_tick_after_lateral_velocity() {
        // A NaN in the guard fails every read, car_name and
        // lateral_velocity included; the dispatcher stores location 0 and lateral_velocity 0, then the top-speed
        // update's speed read ends the tick. The speed is converted only when the integer rpm is 0.
        let before = Telemetry {
            rpm: 3000.0,
            speed: 9.0,
            steering_wheel_angle: 5.0,
            lateral_velocity: 1.0,
            vehicle: "car".into(),
            location: "x".into(),
            ..Telemetry::off()
        };
        let every_read_raised = Telemetry { location: String::new(), lateral_velocity: 0.0, ..before.clone() };
        assert_eq!(polled_over(&before, &packet(f32::NAN, 20.0)), every_read_raised, "int(rpm) raises");
        assert_eq!(polled_over(&before, &packet(0.4, f32::NAN)), every_read_raised, "rpm 0: int(speed) raises");
        // rpm non-zero: the guard passes, only the speed read fails; car_name reads 0
        let side = QUARTER_TURN;
        let only_speed_raised = Telemetry {
            vehicle: String::new(),
            location: String::new(),
            lateral_velocity: -(side.sin() * 3.0 + side.cos() * 4.0),
            ..before.clone()
        };
        assert_eq!(polled_over(&before, &packet(4000.0, f32::NAN)), only_speed_raised);
    }

    #[test]
    fn every_channel_reads_zero_until_a_200_byte_packet() {
        // No telemetry yet: every channel reads 0
        let net = FakeNetwork::default();
        let mut s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &[0u8; 120]);
        assert!(s.is_running().unwrap());
        let mut t =
            Telemetry { rpm: 3000.0, gear: 2.0, vehicle: "car".into(), location: "x".into(), ..Telemetry::off() };
        assert!(!s.poll(&mut t).unwrap());
        let mut zero = Telemetry::off();
        zero_listener_channels(&mut zero);
        assert_eq!(t, zero);
    }

    #[test]
    fn roll_domain_error_keeps_vertical_acceleration() {
        // The arcsine of left_direction_y > 1 fails for roll and vertical_acceleration;
        // the dispatcher sets roll to 0 and keeps vertical_acceleration (default None)
        let mut p = packet(4000.0, 20.0);
        p[52..56].copy_from_slice(&1.5f32.to_le_bytes());
        let net = FakeNetwork::default();
        let mut s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &p);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry { roll: 12.0, vertical_acceleration: 0.8, ..Telemetry::off() };
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.roll, t.vertical_acceleration, t.rpm), (0.0, 0.8, 4000.0));
    }

    #[test]
    fn bind_errors_are_recorded_until_a_datagram_arrives() {
        // Liveness: a bind failure adds its text to the game errors; the receive step empties them
        // after any received datagram, whatever its length
        let net = FakeNetwork::default();
        let mut s = Wrc2023Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        assert!(!s.is_running().unwrap());
        assert!(s.game_errors().is_empty(), "receive timeout");
        net.refuse_bind(true);
        assert!(!s.is_running().unwrap());
        assert_eq!(s.game_errors().iter().collect::<Vec<_>>(), [crate::udp_racing::parameters::PORT_IN_USE]);
        net.refuse_bind(false);
        net.push(DEFAULT_PORT, &[1]);
        assert!(s.is_running().unwrap());
        assert!(!s.poll(&mut Telemetry::off()).unwrap(), "second receive timed out");
        assert_eq!(s.game_errors().len(), 1);
        net.push(DEFAULT_PORT, &[1]);
        net.push(DEFAULT_PORT, &[0u8; 12]);
        assert!(s.is_running().unwrap());
        assert!(!s.poll(&mut Telemetry::off()).unwrap());
        assert!(s.game_errors().is_empty());
    }

    #[test]
    fn only_200_byte_packets_are_decoded() {
        // Receive step: a 200-byte datagram
        let (fresh, t) = polled(&packet(4000.0, 20.0)[..196]);
        assert!(!fresh);
        assert_eq!(t.gear, 0.0, "telemetry is None -> 0");
    }
}

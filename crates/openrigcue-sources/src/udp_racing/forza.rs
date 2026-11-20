//! Forza Motorsport 7 / Forza Motorsport (2023) and Forza Horizon 4/5/6 "Data Out" (listener `forza`).
//!
//! The packet kind is told by its length only: 311 bytes (FM7 "Car Dash"), 331 (FM 2023, the trailing 20
//! bytes are ignored) or 324 (Forza Horizon, the trailing byte is ignored and 12 unknown bytes sit before the
//! dash block).

mod cars;

use std::collections::BTreeSet;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::Telemetry;

use super::forward::Forwarder;
use super::parameters::GameErrors;
pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::{int_text, rad_to_deg, round_to, trunc};
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 10001;
const RECEIVE_SIZE: usize = 336;
const RECEIVE_TIMEOUT: Duration = Duration::from_millis(200);

const MOTORSPORT_LEN: usize = 311;
const MOTORSPORT_2023_LEN: usize = 331;
const HORIZON_LEN: usize = 324;
/// Extra bytes of the Horizon layout between the sled block and the dash block.
const HORIZON_SHIFT: usize = 12;

/// Front-left, front-right, rear-left, rear-right.
type Corners = [f64; 4];

/// The fields of the Motorsport and Horizon packets that the listener uses.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ForzaPacket {
    is_race_on: i32,
    engine_max_rpm: f64,
    current_engine_rpm: f64,
    acceleration: [f64; 3],
    velocity: [f64; 3],
    yaw: f64,
    pitch: f64,
    roll: f64,
    tire_slip_ratio: Corners,
    wheel_on_rumble_strip: Corners,
    suspension_travel_meters: Corners,
    car_ordinal: i32,
    speed: f64,
    fuel: f64,
    gear: u8,
}

fn corners(f: &Fields, offset: usize) -> Result<Corners, Short> {
    Ok([f.f32(offset)?, f.f32(offset + 4)?, f.f32(offset + 8)?, f.f32(offset + 12)?])
}

impl ForzaPacket {
    /// Decodes the fields in use from a Motorsport packet (`dash_shift` 0) or
    /// a Horizon packet (`dash_shift` 12).
    fn parse(data: &[u8], dash_shift: usize) -> Result<Self, Short> {
        let f = Fields::new(data);
        Ok(ForzaPacket {
            is_race_on: f.i32(0)?,
            engine_max_rpm: f.f32(8)?,
            current_engine_rpm: f.f32(16)?,
            acceleration: [f.f32(20)?, f.f32(24)?, f.f32(28)?],
            velocity: [f.f32(32)?, f.f32(36)?, f.f32(40)?],
            yaw: f.f32(56)?,
            pitch: f.f32(60)?,
            roll: f.f32(64)?,
            tire_slip_ratio: corners(&f, 84)?,
            wheel_on_rumble_strip: corners(&f, 116)?,
            suspension_travel_meters: corners(&f, 196)?,
            car_ordinal: f.i32(212)?,
            speed: f.f32(244 + dash_shift)?,
            fuel: f.f32(276 + dash_shift)?,
            gear: f.u8(307 + dash_shift)?,
        })
    }

    /// Chooses the packet kind by the datagram's length.
    fn from_datagram(data: &[u8]) -> Option<Self> {
        match data.len() {
            MOTORSPORT_LEN | MOTORSPORT_2023_LEN => Self::parse(&data[..MOTORSPORT_LEN], 0).ok(),
            HORIZON_LEN => Self::parse(&data[..HORIZON_LEN - 1], HORIZON_SHIFT).ok(),
            _ => None,
        }
    }
}

/// The Forza listener.
pub struct ForzaSource {
    settings: ListenerSettings,
    imported_port: Option<ImportedPort>,
    net: Box<dyn Network>,
    forwarder: Forwarder,
    datagram: Option<Vec<u8>>,
    packet: Option<ForzaPacket>,
    game_errors: GameErrors,
}

impl ForzaSource {
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let forwarder = Forwarder::default();
        ForzaSource {
            settings,
            imported_port: None,
            net,
            forwarder,
            datagram: None,
            packet: None,
            game_errors: GameErrors::default(),
        }
    }

    /// Keep the configured port as an integer until socket creation and bind validation.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        Self { imported_port: Some(port), ..Self::with_network(ListenerSettings::with_port(0), net) }
    }

    /// The error texts this listener contributes to the game errors.
    pub fn game_errors(&self) -> &BTreeSet<String> {
        self.game_errors.messages()
    }

    /// The liveness test: one datagram on a fresh socket, accepted by length. Creation failure only
    /// answers false; a bind or timeout-setting failure records game errors, and receive failure answers false.
    fn receive(&mut self) -> bool {
        let endpoint = Endpoint::any(self.settings.port, RECEIVE_TIMEOUT);
        let socket = match self.imported_port {
            Some(port) => self.net.bind_imported(&endpoint, port),
            None => self.net.bind(&endpoint),
        };
        let mut socket = match socket {
            Ok(socket) => socket,
            Err(e) => {
                if !socket_creation_failed(&e) {
                    self.game_errors.record(&e);
                }
                return false;
            }
        };
        match socket.receive(RECEIVE_SIZE) {
            Ok((data, _)) => {
                self.forwarder.forward(&data);
                let accepted = matches!(data.len(), MOTORSPORT_LEN | HORIZON_LEN | MOTORSPORT_2023_LEN);
                self.datagram = Some(data);
                accepted
            }
            Err(e) => {
                tracing::debug!("forza: {e}");
                false
            }
        }
    }

    /// Writes every channel the dispatcher reads. Before the first packet
    /// there is nothing to read, so every read fails; where a NaN or infinite speed has no integer value the
    /// tick ends after `lateral_velocity` (both [`publish_speed_unreadable`]). Where the rpm or max
    /// rpm has no integer value, the dispatcher keeps the previous value (registry default `None`).
    fn write_channels(&self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let Some(p) = self.packet else {
            return publish_speed_unreadable(t, None, 0.0);
        };
        let vehicle = cars::name(i64::from(p.car_ordinal)).unwrap_or_else(|| p.car_ordinal.to_string());
        let lateral_velocity = round_to(p.velocity[0], 6);
        let Some(speed) = trunc(p.speed) else {
            return publish_speed_unreadable(t, Some(vehicle), lateral_velocity);
        };
        let (rpm, max_rpm) = (t.rpm, t.max_rpm);
        zero_listener_channels(t);
        t.rpm = trunc(p.current_engine_rpm).unwrap_or(rpm);
        t.max_rpm = trunc(p.engine_max_rpm).unwrap_or(max_rpm);
        t.gear = f64::from(p.gear);
        t.speed = speed;
        t.vehicle = vehicle;
        let [fl, fr, rl, rr] = p.tire_slip_ratio;
        (t.wheel_slip_lf, t.wheel_slip_rf, t.wheel_slip_lr, t.wheel_slip_rr) = (fl.abs(), fr.abs(), rl.abs(), rr.abs());
        let [fl, fr, rl, rr] = p.suspension_travel_meters;
        (t.suspension_travel_lf, t.suspension_travel_rf, t.suspension_travel_lr, t.suspension_travel_rr) =
            (fl / 10.0, fr / 10.0, rl / 10.0, rr / 10.0);
        t.lateral_acceleration = -round_to(p.acceleration[0], 6);
        t.vertical_acceleration = round_to(p.acceleration[1], 6);
        t.longitudinal_acceleration = round_to(p.acceleration[2], 6);
        t.yaw = rad_to_deg(p.yaw);
        t.roll = -rad_to_deg(p.roll);
        t.pitch = -rad_to_deg(p.pitch);
        t.lateral_velocity = lateral_velocity;
        t.vertical_velocity = round_to(p.velocity[1], 6);
        t.longitudinal_velocity = round_to(p.velocity[2], 6);
        t.fuel_level = p.fuel * 100.0;
        let rumble = |x: f64| if x > 0.0 { 40.0 } else { 0.0 };
        let [fl, fr, rl, rr] = p.wheel_on_rumble_strip;
        (t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr) =
            (rumble(fl), rumble(fr), rumble(rl), rumble(rr));
        Ok(())
    }
}

impl Source for ForzaSource {
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        if name == "car_name" {
            let ordinal = i64::from(self.packet?.car_ordinal);
            return Some(cars::name(ordinal).map_or(LegacyValue::Int(ordinal), LegacyValue::Str));
        }
        if name == "track_loc" {
            return Some(LegacyValue::Int(0));
        }
        if !Telemetry::CHANNELS.contains(&name) && !matches!(name, "yaw_rate" | "pitch_rate" | "roll_rate") {
            return None;
        }
        let uses_packet = matches!(
            name,
            "rpm"
                | "max_rpm"
                | "gear"
                | "speed"
                | "wheel_slip_lf"
                | "wheel_slip_rf"
                | "wheel_slip_lr"
                | "wheel_slip_rr"
                | "suspension_travel_lf"
                | "suspension_travel_rf"
                | "suspension_travel_lr"
                | "suspension_travel_rr"
                | "lateral_acceleration"
                | "vertical_acceleration"
                | "longitudinal_acceleration"
                | "yaw"
                | "roll"
                | "pitch"
                | "lateral_velocity"
                | "vertical_velocity"
                | "longitudinal_velocity"
                | "fuel_level"
                | "rumble_pitch_lf"
                | "rumble_pitch_rf"
                | "rumble_pitch_lr"
                | "rumble_pitch_rr"
        );
        if !uses_packet {
            return Some(LegacyValue::Int(0));
        }
        let packet = self.packet?;
        let integer_text = |value| int_text(value).map(LegacyValue::Str);
        let integer = |value| {
            trunc(value).map(|value| {
                if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value) {
                    LegacyValue::Int(value as i64)
                } else {
                    // The shared model keeps wider finite integers in its compatibility number view.
                    LegacyValue::Float(value)
                }
            })
        };
        let rumble = |value| LegacyValue::Int(if value > 0.0 { 40 } else { 0 });
        Some(match name {
            "rpm" => return integer_text(packet.current_engine_rpm),
            "max_rpm" => return integer_text(packet.engine_max_rpm),
            "gear" => LegacyValue::Str(packet.gear.to_string()),
            "speed" => return integer(packet.speed),
            "wheel_slip_lf" => LegacyValue::Float(packet.tire_slip_ratio[0].abs()),
            "wheel_slip_rf" => LegacyValue::Float(packet.tire_slip_ratio[1].abs()),
            "wheel_slip_lr" => LegacyValue::Float(packet.tire_slip_ratio[2].abs()),
            "wheel_slip_rr" => LegacyValue::Float(packet.tire_slip_ratio[3].abs()),
            "suspension_travel_lf" => LegacyValue::Float(packet.suspension_travel_meters[0] / 10.0),
            "suspension_travel_rf" => LegacyValue::Float(packet.suspension_travel_meters[1] / 10.0),
            "suspension_travel_lr" => LegacyValue::Float(packet.suspension_travel_meters[2] / 10.0),
            "suspension_travel_rr" => LegacyValue::Float(packet.suspension_travel_meters[3] / 10.0),
            "lateral_acceleration" => LegacyValue::Float(-round_to(packet.acceleration[0], 6)),
            "vertical_acceleration" => LegacyValue::Float(round_to(packet.acceleration[1], 6)),
            "longitudinal_acceleration" => LegacyValue::Float(round_to(packet.acceleration[2], 6)),
            "yaw" => LegacyValue::Float(rad_to_deg(packet.yaw)),
            "roll" => LegacyValue::Float(-rad_to_deg(packet.roll)),
            "pitch" => LegacyValue::Float(-rad_to_deg(packet.pitch)),
            "lateral_velocity" => LegacyValue::Float(round_to(packet.velocity[0], 6)),
            "vertical_velocity" => LegacyValue::Float(round_to(packet.velocity[1], 6)),
            "longitudinal_velocity" => LegacyValue::Float(round_to(packet.velocity[2], 6)),
            "fuel_level" => LegacyValue::Float(packet.fuel * 100.0),
            "rumble_pitch_lf" => rumble(packet.wheel_on_rumble_strip[0]),
            "rumble_pitch_rf" => rumble(packet.wheel_on_rumble_strip[1]),
            "rumble_pitch_lr" => rumble(packet.wheel_on_rumble_strip[2]),
            "rumble_pitch_rr" => rumble(packet.wheel_on_rumble_strip[3]),
            _ => unreachable!("packet channels checked above"),
        })
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        let Some(packet) = self.packet else { return self.native_channel(name).is_none() };
        match name {
            "rpm" => !packet.current_engine_rpm.is_finite(),
            "max_rpm" => !packet.engine_max_rpm.is_finite(),
            "speed" => !packet.speed.is_finite(),
            _ => false,
        }
    }

    fn id(&self) -> &'static str {
        "forza"
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.receive())
    }

    /// The receive step: decode the last datagram; it counts as fresh, and clears the game errors,
    /// only while `is_race_on`.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let decoded = self.datagram.as_deref().and_then(ForzaPacket::from_datagram);
        if let Some(packet) = decoded {
            self.packet = Some(packet);
        }
        let race_on = decoded.is_some_and(|p| p.is_race_on != 0);
        if race_on {
            self.game_errors.clear();
        }
        Ok(race_on)
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry)?;
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;
    use crate::udp_racing::unhandled::assert_ends;

    #[test]
    fn native_forza_imported_port_retries_without_losing_the_prior_packet() {
        use crate::udp_racing::support::net::Datagrams;
        use std::sync::{Arc, Mutex};
        struct Staged {
            calls: Arc<Mutex<usize>>,
            good: FakeNetwork,
        }
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn Datagrams>> {
                panic!("native path")
            }
            fn bind_imported(
                &mut self,
                endpoint: &Endpoint,
                port: ImportedPort,
            ) -> std::io::Result<Box<dyn Datagrams>> {
                assert_eq!(port, ImportedPort::Valid(6001));
                assert_eq!(endpoint.bind.ip(), &std::net::Ipv4Addr::UNSPECIFIED);
                assert_eq!(endpoint.timeout, Some(RECEIVE_TIMEOUT));
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                match *calls {
                    1 => Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other(
                        "allocation denied",
                    ))),
                    2 => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "bind(): port must be 0-65535.")),
                    3 => Err(std::io::Error::other("timeout setting denied")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(6001);
                        self.good.bind(&endpoint)
                    }
                }
            }
        }
        let calls = Arc::new(Mutex::new(0));
        let good = FakeNetwork::default();
        good.push(6001, &packet(MOTORSPORT_LEN, 0));
        let mut source = ForzaSource::with_imported_network(
            ImportedPort::Valid(6001),
            Box::new(Staged { calls: calls.clone(), good }),
        );
        source.datagram = Some(b"old datagram".to_vec());
        source.packet = ForzaPacket::from_datagram(&packet(MOTORSPORT_LEN, 0));
        for expected_count in [0, 1, 2] {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.game_errors().len(), expected_count);
            assert_eq!(source.datagram.as_deref(), Some(b"old datagram".as_slice()));
            assert_eq!(source.packet.unwrap().gear, 4);
        }
        assert!(source.is_running().unwrap());
        source.receive_native(&mut Telemetry::off()).unwrap();
        assert!(source.game_errors().is_empty());
        assert_eq!(*calls.lock().unwrap(), 4);
    }

    #[test]
    fn native_forza_creation_failure_keeps_prior_packet_without_game_error() {
        struct Failure;
        impl Network for Failure {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn super::super::support::net::Datagrams>> {
                Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("create failed")))
            }
        }
        let prior = packet(MOTORSPORT_LEN, 0);
        let mut source = ForzaSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(Failure));
        source.datagram = Some(prior.clone());
        source.packet = ForzaPacket::from_datagram(&prior);
        assert!(!source.is_running().unwrap());
        assert!(source.game_errors().is_empty(), "creation is caught separately from bind/settimeout");
        assert_eq!(source.datagram, Some(prior));
        assert_eq!(source.packet.unwrap().current_engine_rpm, f64::from(4321.9_f32));
    }

    fn put(p: &mut [u8], off: usize, v: f32) {
        p[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A "Car Dash" packet in the Data Out layout.
    fn packet(len: usize, shift: usize) -> Vec<u8> {
        let mut p = vec![0u8; len];
        p[0..4].copy_from_slice(&1i32.to_le_bytes());
        put(&mut p, 8, 8000.0);
        put(&mut p, 16, 4321.9);
        put(&mut p, 20, 1.2345674);
        put(&mut p, 24, -2.5);
        put(&mut p, 28, 3.0);
        put(&mut p, 32, 0.5);
        put(&mut p, 36, 0.25);
        put(&mut p, 40, 30.0);
        put(&mut p, 56, std::f32::consts::PI);
        put(&mut p, 60, 0.0);
        put(&mut p, 64, 0.0);
        put(&mut p, 84, -0.5);
        put(&mut p, 120, 1.0);
        put(&mut p, 196, 0.2);
        p[212..216].copy_from_slice(&2489i32.to_le_bytes());
        put(&mut p, 244 + shift, 30.9);
        put(&mut p, 276 + shift, 0.75);
        p[307 + shift] = 4;
        p
    }

    fn running_source(datagram: &[u8]) -> (ForzaSource, FakeNetwork) {
        let net = FakeNetwork::default();
        let mut s = ForzaSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, datagram);
        let _ = s.is_running().unwrap();
        (s, net)
    }

    #[test]
    fn native_receive_defers_nonfinite_speed_getter_and_preserves_raw_identity() {
        use crate::derive::{Listener, SourceListener};
        use crate::NativeRead;

        let mut data = packet(MOTORSPORT_LEN, 0);
        put(&mut data, 244, f32::INFINITY);
        data[212..216].copy_from_slice(&987654_i32.to_le_bytes());
        let (mut source, _) = running_source(&data);
        let before = Telemetry { speed: 17.0, rpm: 2000.0, vehicle: "prior".into(), ..Telemetry::off() };
        let mut compatibility = before.clone();
        assert!(source.receive_native(&mut compatibility).unwrap());
        assert_eq!(compatibility, before, "receiveData only stores the packet");
        assert_eq!(source.read_native("speed"), NativeRead::Raised);
        assert_eq!(source.read_native("car_name"), NativeRead::Value(LegacyValue::Int(987654)));

        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.failure_point(), None, "a getter failure is not a receive failure");
        assert_eq!(listener.read_native("speed"), NativeRead::Raised);
    }

    #[test]
    fn native_forza_udp_packets_update_getters_without_projecting_the_dispatch_frame() {
        use crate::udp_flight::transport::testing::{free_port, Sender};

        let port = free_port();
        let mut source = ForzaSource::new(ListenerSettings::with_port(port));
        let before = Telemetry { speed: 7.0, rpm: 11.0, vehicle: "previous".into(), ..Telemetry::off() };
        let mut compatibility = before.clone();
        for (len, shift, race_on) in
            [(MOTORSPORT_LEN, 0, true), (HORIZON_LEN, HORIZON_SHIFT, true), (MOTORSPORT_2023_LEN, 0, false)]
        {
            let mut bytes = packet(len, shift);
            bytes[..4].copy_from_slice(&i32::from(race_on).to_le_bytes());
            let sender = Sender::start(port, bytes);
            assert!(source.is_running().unwrap());
            drop(sender);
            assert_eq!(source.receive_native(&mut compatibility).unwrap(), race_on);
            assert_eq!(compatibility, before);
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Int(30)));
            assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Str("4321".into())));
            assert_eq!(source.native_channel("car_name"), cars::name(2489).map(LegacyValue::Str));
        }
    }

    #[test]
    fn invalid_speed_reaches_current_profile_lookup_at_the_original_dispatch_stage() {
        use crate::derive::{
            AttitudeSource, DamsMode, DamsSettings, DispatchPublication, DispatchSettings, Dispatcher, GameDirectory,
            IntellituneReference, ProfileSettings, RefreshRates, RuntimeInputs, ScanHit, SourceListener, SpeedUnit,
            TickInput,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        struct Inputs(AtomicUsize);
        impl DispatchPublication for Inputs {
            fn assigned(&self, _: &'static str, _: &Telemetry) {}
            fn profile_settings(&self) -> Option<ProfileSettings> {
                self.0.fetch_add(1, Ordering::SeqCst);
                None
            }
        }
        struct Games;
        impl GameDirectory for Games {
            fn is_flying(&self, _: &str) -> bool {
                false
            }
            fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
                None
            }
        }
        let mut data = packet(MOTORSPORT_LEN, 0);
        put(&mut data, 244, f32::INFINITY);
        data[212..216].copy_from_slice(&987654_i32.to_le_bytes());
        let (source, net) = running_source(&data);
        net.push(DEFAULT_PORT, &data);
        let mut dispatcher = Dispatcher::new(vec![Box::new(SourceListener::new(source))]);
        let inputs = Arc::new(Inputs(AtomicUsize::new(0)));
        dispatcher.set_publication(inputs.clone());
        let settings = DispatchSettings {
            dams: DamsSettings {
                mode: DamsMode::Off,
                factor: Some(1.0),
                front_lever: 0.0,
                rear_lever: 1.0,
                steering_ratio: Some(1.0),
            },
            refresh_rates: RefreshRates { shakebox: 30, intellibox: 30, motionbox: 30 },
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
        dispatcher.tick(&TickInput {
            hits: &[ScanHit { game_id: "forzahorizon5".into(), listener_id: "forza".into() }],
            settings: &settings,
            profile: &profile,
            runtime: &RuntimeInputs::default(),
            games: &Games,
        });
        assert_eq!(
            inputs.0.load(Ordering::SeqCst),
            1,
            "getMaxSpeed reads the current profile before its speed getter fails"
        );
        assert!(dispatcher.has_ended());
        assert_eq!(dispatcher.vehicle_value(), &LegacyValue::Int(987654));
        assert_eq!(dispatcher.telemetry().lateral_velocity, 0.5);
        assert_eq!(dispatcher.telemetry().rpm, Telemetry::off().rpm, "later auto-load never ran");
    }

    #[test]
    fn native_getters_retain_source_text_integer_and_float_answers() {
        use crate::derive::{Listener, SourceListener};
        use openrigcue_model::native_value::LegacyValue::{Float, Int, Str};
        let (source, _) = running_source(&packet(MOTORSPORT_LEN, 0));
        let mut listener = SourceListener::new(source);
        listener.receive();
        for (name, expected) in [
            ("rpm", Str("4321".into())),
            ("max_rpm", Str("8000".into())),
            ("gear", Str("4".into())),
            ("speed", Int(30)),
            ("wheel_slip_lf", Float(0.5)),
            ("lateral_acceleration", Float(-1.234567)),
            ("vertical_velocity", Float(0.25)),
            ("fuel_level", Float(75.0)),
            ("rumble_pitch_lf", Int(0)),
            ("rumble_pitch_rf", Int(40)),
            ("raw_flag", Int(0)),
            ("steering_wheel_angle", Int(0)),
            ("yaw_rate", Int(0)),
        ] {
            assert_eq!(listener.native_channel(name), Some(expected), "{name}");
        }
        assert_eq!(listener.channel("rpm"), None, "native receive does not precompute the compatibility frame");
    }

    #[test]
    fn native_integer_text_normalizes_negative_zero_and_keeps_wide_finite_magnitudes() {
        let mut data = packet(MOTORSPORT_LEN, 0);
        put(&mut data, 16, -0.9);
        put(&mut data, 8, 18_446_744_073_709_551_616.0);
        let (mut source, _) = running_source(&data);
        source.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Str("0".into())));
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Str("18446744073709551616".into())));
    }

    #[test]
    fn failed_rpm_getters_are_unavailable_instead_of_reusing_the_adapter_frame() {
        use crate::derive::{Listener, SourceListener};
        let (source, network) = running_source(&packet(MOTORSPORT_LEN, 0));
        let mut listener = SourceListener::new(source);
        listener.receive();
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut data = packet(MOTORSPORT_LEN, 0);
            put(&mut data, 16, invalid);
            put(&mut data, 8, invalid);
            network.push(DEFAULT_PORT, &data);
            assert!(listener.is_running());
            listener.receive();
            assert_eq!(listener.native_channel("rpm"), None);
            assert_eq!(listener.native_channel("max_rpm"), None);
            assert_eq!(
                listener.channel("rpm"),
                None,
                "int({invalid}) raised; the dispatcher keeps its own prior value"
            );
            assert_eq!(listener.channel("max_rpm"), None);
            assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Int(30)));
        }
    }

    #[test]
    fn check_status_accepts_only_known_lengths() {
        // Liveness: the length is 311, 324 or 331; bind 0.0.0.0, timeout 0.2, 336-byte receive
        for (len, ok) in [(311, true), (324, true), (331, true), (232, false), (336, false)] {
            let net = FakeNetwork::default();
            let mut s = ForzaSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
            net.push(DEFAULT_PORT, &vec![0u8; len]);
            assert_eq!(s.is_running().unwrap(), ok, "len {len}");
            assert_eq!(net.bound()[0].bind, "0.0.0.0:10001".parse().unwrap());
            assert_eq!(net.bound()[0].timeout, Some(Duration::from_millis(200)));
        }
        let net = FakeNetwork::default();
        let mut s = ForzaSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &vec![0u8; 337]);
        assert!(!s.is_running().unwrap(), "larger than the receive buffer");
    }

    #[test]
    fn motorsport_channels() {
        // Channels of a Motorsport packet
        let (mut s, _) = running_source(&packet(311, 0));
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (4321.0, 8000.0, 4.0, 30.0));
        assert_eq!(t.vehicle, "2016 Abarth 695 Biposto");
        assert_eq!(t.wheel_slip_lf, 0.5);
        assert_eq!(t.suspension_travel_lf, f64::from(0.2f32) / 10.0);
        assert_eq!(t.lateral_acceleration, -1.234567);
        assert_eq!(t.vertical_acceleration, -2.5);
        assert_eq!(t.longitudinal_acceleration, 3.0);
        assert_eq!(t.lateral_velocity, 0.5);
        assert_eq!(t.vertical_velocity, 0.25);
        assert_eq!(t.longitudinal_velocity, 30.0);
        assert!((t.yaw - 180.0).abs() < 1e-4);
        assert_eq!(t.fuel_level, 75.0);
        assert_eq!((t.rumble_pitch_lf, t.rumble_pitch_rf), (0.0, 40.0));
        assert_eq!(t.steering_wheel_angle, 0.0, "unknown channels read 0");
    }

    #[test]
    fn horizon_and_2023_layouts() {
        // Receive step: the first 323 bytes are a Horizon packet; the first 311 of a 331-byte datagram a Motorsport one
        let (mut s, _) = running_source(&packet(324, 12));
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.speed, t.gear, t.fuel_level), (30.0, 4.0, 75.0));
        let (mut s, _) = running_source(&packet(331, 0));
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.gear, 4.0);
    }

    #[test]
    fn race_off_is_not_fresh_but_still_read() {
        // Receive step: fresh only while the race is on; channels still use the packet
        let mut p = packet(311, 0);
        p[0] = 0;
        let (mut s, _) = running_source(&p);
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 4321.0);
    }

    #[test]
    fn unknown_car_reports_its_ordinal() {
        // car_name: an unknown ordinal answers the raw ordinal
        let mut p = packet(311, 0);
        p[212..216].copy_from_slice(&7i32.to_le_bytes());
        let (mut s, _) = running_source(&p);
        let mut t = Telemetry::off();
        s.poll(&mut t).unwrap();
        assert_eq!(t.vehicle, "7");
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Int(7)));
        s.packet = ForzaPacket::from_datagram(&packet(MOTORSPORT_LEN, 0));
        assert_eq!(s.native_channel("car_name"), cars::name(2489).map(LegacyValue::Str));
    }

    #[test]
    fn non_finite_rpm_keeps_the_previous_value() {
        // A NaN has no integer value; the dispatcher keeps rpm/max_rpm (default None)
        let mut p = packet(311, 0);
        put(&mut p, 16, f32::NAN);
        put(&mut p, 8, f32::INFINITY);
        let (mut s, _) = running_source(&p);
        let mut t = Telemetry { rpm: 2500.0, max_rpm: 7000.0, speed: 11.0, ..Telemetry::off() };
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.speed, t.gear), (2500.0, 7000.0, 30.0, 4.0));
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // A NaN speed has no integer value; the dispatcher's top-speed update
        // does not handle that, so only car_name, track_loc (0) and lateral_velocity are stored
        let mut p = packet(311, 0);
        put(&mut p, 244, f32::NAN);
        let (mut s, _) = running_source(&p);
        let before = Telemetry { rpm: 2500.0, speed: 11.0, pitch: 3.0, location: "x".into(), ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        let vehicle = cars::name(2489).unwrap();
        let lateral_velocity = round_to(f64::from(0.5f32), 6);
        assert_eq!(t, Telemetry { vehicle, location: String::new(), lateral_velocity, ..before });
    }

    #[test]
    fn before_the_first_packet_every_read_raises() {
        // Channel reads need the packet, which only the receive step stores: before it every
        // channel read fails, car_name included; the dispatcher stores location 0 and lateral_velocity 0
        let (mut s, _) = running_source(&[0u8; 232]);
        let before = Telemetry {
            rpm: 2500.0,
            lateral_velocity: 4.0,
            vehicle: "car".into(),
            location: "x".into(),
            ..Telemetry::off()
        };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { location: String::new(), lateral_velocity: 0.0, ..before });
    }

    #[test]
    fn socket_errors_are_recorded_until_the_race_is_on() {
        // A bind failure adds its text to the game errors; a receive failure does not.
        // The receive step clears them while the race is on
        let net = FakeNetwork::default();
        let mut s = ForzaSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        assert!(!s.is_running().unwrap());
        assert!(s.game_errors().is_empty(), "timeout");
        net.refuse_bind(true);
        assert!(!s.is_running().unwrap());
        assert_eq!(s.game_errors().iter().collect::<Vec<_>>(), [crate::udp_racing::parameters::PORT_IN_USE]);
        net.refuse_bind(false);
        let mut off = packet(311, 0);
        off[0] = 0;
        net.push(DEFAULT_PORT, &off);
        assert!(s.is_running().unwrap());
        assert!(!s.poll(&mut Telemetry::off()).unwrap());
        assert_eq!(s.game_errors().len(), 1, "race not on");
        net.push(DEFAULT_PORT, &packet(311, 0));
        assert!(s.is_running().unwrap());
        assert!(s.poll(&mut Telemetry::off()).unwrap());
        assert!(s.game_errors().is_empty());
    }

    #[test]
    fn nothing_received_is_not_fresh() {
        // The receive step with no datagram: the length test fails -> false
        let net = FakeNetwork::default();
        let mut s = ForzaSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net));
        let mut t = Telemetry::off();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry::off());
    }
}

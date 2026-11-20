//! Wreckfest 2 "PIN0" UDP telemetry, main packet only (listener `wreckfest2`).
//!
//! The packet is a byte-packed structure of 1218 bytes, including
//! its 14-byte header. Every channel is evaluated on demand, and a 50-entry rpm history is kept across
//! channel reads to detect a frozen stream. Typed dispatch advances it at each actual getter; direct numeric
//! polling retains a compatibility replay.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::{is_listener_read, publish_speed_unreadable, zero_listener_channels};
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::trunc;
use super::support::quaternion;
use super::support::receiver::Receiver;
use super::support::wire::{decode_utf8_ignoring_errors, Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::derive::CHANNEL_SPECS;
use crate::{NativeRead, Source, SourceError};

/// `NETWORK_PORT.wreckfest2` as seeded by configuration migration 381.
/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 23123;
const RECEIVE_SIZE: usize = 65535;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
/// The PIN0 signature (the bytes `pkro`).
const SIGNATURE: u32 = 0x6F72_6B70;
/// The main packet type.
const PACKET_MAIN: u8 = 0;
/// Size of the main packet.
const PACKET_MAIN_LEN: usize = 1218;
/// Game status flags.
const PAUSED: u8 = 1;
const REPLAY: u8 = 2;
const SPECTATE: u8 = 4;
/// The low- and high-frequency rumble surface types.
const CURB_SURFACES: [u8; 2] = [7, 8];
/// Length of the rpm history.
const RPM_HISTORY: usize = 50;

/// Offsets inside the main packet.
mod layout {
    pub const STATUS_FLAGS: usize = 5;
    pub const CAR_NAME: usize = 116 + 64;
    pub const CAR_NAME_LEN: usize = 96;
    const CAR: usize = 350;
    pub const DRIVELINE_GEAR: usize = CAR + 61;
    pub const DRIVELINE_SPEED: usize = CAR + 63;
    pub const ENGINE_RPM: usize = CAR + 85;
    pub const ENGINE_RPM_MAX: usize = CAR + 89;
    pub const QUATERNION: usize = CAR + 172;
    pub const VELOCITY_LOCAL: usize = CAR + 194;
    pub const ACCELERATION_LOCAL: usize = CAR + 218;
    pub const TIRES: usize = CAR + 230;
    pub const TIRE_LEN: usize = 72;
    pub const TIRE_SUSPENSION_DISPLACEMENT: usize = 44;
    pub const TIRE_SURFACE_TYPE: usize = 56;
}

/// Tire order: front-left, front-right, rear-left, rear-right.
const FL: usize = 0;
const FR: usize = 1;
const RL: usize = 2;
const RR: usize = 3;

/// The main packet fields the listener uses.
#[derive(Debug, Clone, PartialEq)]
struct MainPacket {
    status_flags: u8,
    car_name: String,
    gear: u8,
    speed: f64,
    rpm: i32,
    rpm_max: i32,
    /// `qx, qy, qz, qw`.
    quaternion: [f64; 4],
    velocity_local: [f64; 3],
    acceleration_local: [f64; 3],
    suspension_displacement: [f64; 4],
    surface: [u8; 4],
}

impl MainPacket {
    /// Decodes a datagram of at least the main packet size.
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        f.bytes(0, PACKET_MAIN_LEN)?;
        let v3 = |o: usize| -> Result<[f64; 3], Short> { Ok([f.f32(o)?, f.f32(o + 4)?, f.f32(o + 8)?]) };
        let tire = |i: usize, field: usize| layout::TIRES + i * layout::TIRE_LEN + field;
        let mut suspension_displacement = [0.0; 4];
        let mut surface = [0u8; 4];
        for i in 0..4 {
            suspension_displacement[i] = f.f32(tire(i, layout::TIRE_SUSPENSION_DISPLACEMENT))?;
            surface[i] = f.u8(tire(i, layout::TIRE_SURFACE_TYPE))?;
        }
        let q = layout::QUATERNION;
        Ok(MainPacket {
            status_flags: f.u8(layout::STATUS_FLAGS)?,
            car_name: c_char_array(f.bytes(layout::CAR_NAME, layout::CAR_NAME_LEN)?),
            gear: f.u8(layout::DRIVELINE_GEAR)?,
            speed: f.f32(layout::DRIVELINE_SPEED)?,
            rpm: f.i32(layout::ENGINE_RPM)?,
            rpm_max: f.i32(layout::ENGINE_RPM_MAX)?,
            quaternion: [f.f32(q)?, f.f32(q + 4)?, f.f32(q + 8)?, f.f32(q + 12)?],
            velocity_local: v3(layout::VELOCITY_LOCAL)?,
            acceleration_local: v3(layout::ACCELERATION_LOCAL)?,
            suspension_displacement,
            surface,
        })
    }
}

/// The text of a fixed-size character array.
fn c_char_array(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    decode_utf8_ignoring_errors(&raw[..end])
}

/// The dispatcher settings that decide which channel reads one dispatcher tick makes
///: the DAMS mode and the profile's pitch and roll sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatcherReads {
    /// `CUSTOM_FEATURES.dams_front_tl_mode` as an integer: mode 2 reads `steering_wheel_angle` and
    /// `yaw_rate` (front) and `yaw_rate` instead of `lateral_velocity` (rear).
    pub dams_front_tl_mode: i64,
    /// Whether the profile's `motion_effects.pitch_type` is `"angle"` (`"angle"` when the key is missing): `pitch`
    /// is read, otherwise `pitch_rate`.
    pub pitch_angle: bool,
    /// Whether the profile's `motion_effects.roll_type` is `"angle"`: `roll`, otherwise `roll_rate`.
    pub roll_angle: bool,
}

impl Default for DispatcherReads {
    /// Configuration defaults: `dams_front_tl_mode` 0, angle-type pitch and roll.
    fn default() -> Self {
        DispatcherReads { dams_front_tl_mode: 0, pitch_angle: true, roll_angle: true }
    }
}

/// The dispatcher's DAMS speed test: the speed must exceed 0.277778.
const DAMS_MIN_SPEED: f64 = 0.277778;

/// The Wreckfest 2 listener.
pub struct Wreckfest2Source {
    receiver: Receiver,
    /// `APP.pause_telemetry_when_not_playing`.
    pause_when_not_playing: bool,
    pause_reader: Option<crate::shared_memory::NativePauseReader>,
    datagram: Vec<u8>,
    packet: Option<MainPacket>,
    rpm_history: Mutex<VecDeque<i32>>,
    /// What decides the dispatcher's channel reads of a tick.
    reads: DispatcherReads,
    /// The channel reads made, in order (tests).
    #[cfg(test)]
    calls: Vec<String>,
}

impl Wreckfest2Source {
    pub fn new(settings: ListenerSettings, pause_when_not_playing: bool) -> Self {
        Self::with_network(settings, pause_when_not_playing, Box::new(SystemNetwork))
    }

    pub fn from_imported_port(port: ImportedPort, pause_when_not_playing: bool) -> Self {
        Self::with_imported_network(port, pause_when_not_playing, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, pause_when_not_playing: bool, net: Box<dyn Network>) -> Self {
        Self::with_receiver(
            Receiver::with_imported_port(net, Endpoint::loopback(0, RECEIVE_TIMEOUT), RECEIVE_SIZE, port),
            pause_when_not_playing,
        )
    }

    fn with_network(settings: ListenerSettings, pause_when_not_playing: bool, net: Box<dyn Network>) -> Self {
        let endpoint = Endpoint::loopback(settings.port, RECEIVE_TIMEOUT);
        Self::with_receiver(Receiver::new(net, endpoint, RECEIVE_SIZE), pause_when_not_playing)
    }

    fn with_receiver(receiver: Receiver, pause_when_not_playing: bool) -> Self {
        Wreckfest2Source {
            receiver,
            pause_when_not_playing,
            pause_reader: None,
            datagram: Vec::new(),
            packet: None,
            rpm_history: Mutex::new(VecDeque::with_capacity(RPM_HISTORY + 1)),
            reads: DispatcherReads::default(),
            #[cfg(test)]
            calls: Vec::new(),
        }
    }

    /// The dispatcher settings that decide the channel reads of a tick (read on every
    /// tick from the configuration and the game profile).
    pub fn set_dispatcher_reads(&mut self, reads: DispatcherReads) {
        self.reads = reads;
    }

    /// Numeric compatibility pause flag; typed getters use the optional current setting reader.
    pub fn set_pause_when_not_playing(&mut self, pause: bool) {
        self.pause_when_not_playing = pause;
    }

    pub fn set_pause_reader(&mut self, reader: crate::shared_memory::NativePauseReader) {
        self.pause_reader = Some(reader);
    }

    /// Whether the game state blocks telemetry: paused always blocks; replay and spectate block when the user pauses
    /// telemetry outside of play.
    fn should_block(&self, p: &MainPacket) -> bool {
        p.status_flags & PAUSED != 0 || (self.pause_when_not_playing && p.status_flags & (REPLAY | SPECTATE) != 0)
    }

    /// Whether the stream is frozen: 50 identical rpm samples at zero speed mean a frozen stream.
    fn is_stuck(&self, speed: f64, rpm: i32) -> bool {
        let mut history = self.rpm_history.lock().unwrap_or_else(|e| e.into_inner());
        history.push_back(rpm);
        if history.len() > RPM_HISTORY {
            history.pop_front();
        }
        history.len() == RPM_HISTORY && speed == 0.0 && history.iter().all(|&r| r == rpm)
    }

    /// The checks every channel read makes before answering: no packet, a blocked state or a frozen
    /// stream answer 0. The frozen-stream test records the rpm, so each call counts.
    fn gate(&self) -> Option<MainPacket> {
        let p = self.packet.clone()?;
        if self.should_block(&p) || self.is_stuck(p.speed, p.rpm) {
            return None;
        }
        Some(p)
    }

    fn native_gate(&self) -> Option<MainPacket> {
        let p = self.packet.clone()?;
        if p.status_flags & PAUSED != 0 {
            return None;
        }
        // The required setting is evaluated even in live play. A failed read of it
        // answers integer zero before the frozen-stream test can append history.
        let pause = crate::shared_memory::native_pause(&self.pause_reader, self.pause_when_not_playing)?;
        if (pause && p.status_flags & (REPLAY | SPECTATE) != 0) || self.is_stuck(p.speed, p.rpm) {
            return None;
        }
        Some(p)
    }

    /// One channel read of a numeric channel.
    fn read(&mut self, channel: &str) -> f64 {
        #[cfg(test)]
        self.calls.push(channel.to_string());
        self.gate().map_or(0.0, |p| channel_value(&p, channel))
    }

    /// One read of `car_name`.
    fn read_car_name(&mut self) -> String {
        #[cfg(test)]
        self.calls.push("car_name".to_string());
        self.gate().map(|p| car_name(p.car_name)).unwrap_or_default()
    }

    /// The dispatcher's channel reads of one tick in order,
    /// so that the rpm history grows by one sample per read:
    /// `car_name`, `track_loc`, `speed` (read again when the integer value of the first is not negative),
    /// `lateral_velocity`, `longitudinal_velocity`, the DAMS reads of [`DispatcherReads`], `speed` again in
    /// the top-speed update, every auto-loaded registry channel, `yaw` twice, then pitch and roll (or their rates).
    ///
    /// Where the speed has no integer value, the first test only logs and the top-speed update ends the
    /// dispatcher: the reads stop there and [`DispatcherEnds`] is returned.
    fn write_channels(&mut self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        let name = self.read_car_name();
        self.read("track_loc");
        let first_speed = self.read("speed");
        // The stored speed after the first test; it keeps the previous value when the conversion fails.
        let (speed, stored_speed) = match trunc(first_speed) {
            Some(first) if first < 0.0 => (0.0, 0.0),
            Some(_) => {
                let speed = self.read("speed");
                (speed, trunc(speed).unwrap_or(t.speed))
            }
            None => (t.speed, t.speed),
        };
        let lateral_read = self.read("lateral_velocity");
        let longitudinal_read = self.read("longitudinal_velocity");
        let fast = stored_speed > DAMS_MIN_SPEED;
        let mode2 = self.reads.dams_front_tl_mode == 2;
        if mode2 && longitudinal_read != 0.0 {
            self.read("steering_wheel_angle");
            if fast {
                self.read("yaw_rate");
            }
        }
        let lateral_velocity = if mode2 {
            if longitudinal_read != 0.0 && fast {
                self.read("yaw_rate");
            }
            lateral_read
        } else {
            self.read("lateral_velocity")
        };
        // Top-speed update: the integer speed; a failure is not handled.
        if trunc(self.read("speed")).is_none() {
            return publish_speed_unreadable(t, Some(name), lateral_velocity);
        }
        zero_listener_channels(t);
        t.vehicle = name;
        t.speed = speed;
        t.lateral_velocity = lateral_velocity;
        for spec in CHANNEL_SPECS.iter().filter(|spec| spec.auto_load) {
            let value = self.read(spec.name);
            if is_listener_read(spec.name) {
                if let Some(slot) = t.get_mut(spec.name) {
                    *slot = value;
                }
            }
        }
        t.yaw = self.read("yaw");
        self.read("yaw");
        let pitch = self.read(if self.reads.pitch_angle { "pitch" } else { "pitch_rate" });
        if self.reads.pitch_angle {
            t.pitch = pitch;
        }
        let roll = self.read(if self.reads.roll_angle { "roll" } else { "roll_rate" });
        if self.reads.roll_angle {
            t.roll = roll;
        }
        Ok(())
    }
}

/// The car name: the literal string "None" becomes empty text.
fn car_name(name: String) -> String {
    if name == "None" {
        String::new()
    } else {
        name
    }
}

fn native_value(p: &MainPacket, channel: &str) -> LegacyValue {
    match channel {
        "rpm" => LegacyValue::Int(i64::from(p.rpm)),
        "max_rpm" => LegacyValue::Int(i64::from(p.rpm_max)),
        "gear" => LegacyValue::Int(i64::from(p.gear) - 1),
        "car_name" => LegacyValue::Str(car_name(p.car_name.clone())),
        "rumble_pitch_lf" | "rumble_pitch_rf" | "rumble_pitch_lr" | "rumble_pitch_rr" => {
            LegacyValue::Int(channel_value(p, channel) as i64)
        }
        "speed"
        | "yaw"
        | "roll"
        | "pitch"
        | "longitudinal_acceleration"
        | "lateral_acceleration"
        | "vertical_acceleration"
        | "longitudinal_velocity"
        | "lateral_velocity"
        | "vertical_velocity"
        | "suspension_travel_lf"
        | "suspension_travel_lr"
        | "suspension_travel_rf"
        | "suspension_travel_rr" => LegacyValue::Float(channel_value(p, channel)),
        _ => LegacyValue::Int(0),
    }
}

/// The channel values once the gate passed. Roll and pitch are the negated quaternion angles;
/// yaw is not negated.
fn channel_value(p: &MainPacket, channel: &str) -> f64 {
    let [qx, qy, qz, qw] = p.quaternion;
    let curb = |i: usize| if CURB_SURFACES.contains(&p.surface[i]) { 40.0 } else { 0.0 };
    match channel {
        "speed" => p.speed,
        "rpm" => f64::from(p.rpm),
        "max_rpm" => f64::from(p.rpm_max),
        "gear" => f64::from(p.gear) - 1.0,
        "yaw" => quaternion::yaw(qx, qy, qz, qw),
        "roll" => -quaternion::roll(qx, qy, qz, qw),
        "pitch" => -quaternion::pitch(qx, qy, qz, qw),
        "longitudinal_acceleration" => p.acceleration_local[0],
        "lateral_acceleration" => p.acceleration_local[1],
        "vertical_acceleration" => p.acceleration_local[2],
        "longitudinal_velocity" => p.velocity_local[0] * 100.0,
        "lateral_velocity" => p.velocity_local[1] * 100.0,
        "vertical_velocity" => p.velocity_local[2] * 100.0,
        "suspension_travel_lf" => p.suspension_displacement[FL],
        "suspension_travel_lr" => p.suspension_displacement[RL],
        "suspension_travel_rf" => p.suspension_displacement[FR],
        "suspension_travel_rr" => p.suspension_displacement[RR],
        "rumble_pitch_lf" => curb(FL),
        "rumble_pitch_rf" => curb(FR),
        "rumble_pitch_lr" => curb(RL),
        "rumble_pitch_rr" => curb(RR),
        _ => 0.0,
    }
}

impl Source for Wreckfest2Source {
    fn id(&self) -> &'static str {
        "wreckfest2"
    }

    fn read_native(&self, name: &str) -> NativeRead {
        NativeRead::Value(self.native_gate().map_or(LegacyValue::Int(0), |p| native_value(&p, name)))
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    /// The liveness test: a datagram whose header carries the PIN0 signature and the main packet type.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let running = match self.receiver.receive_once() {
            Ok(data) => {
                let f = Fields::new(&data);
                let header_ok = matches!((f.u32(0), f.u8(4), f.bytes(0, 14)), (Ok(SIGNATURE), Ok(PACKET_MAIN), Ok(_)));
                self.datagram = data;
                header_ok
            }
            Err(_) => false,
        };
        Ok(running)
    }

    /// The receive step: decode the main packet when the datagram is long enough.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let parsed = MainPacket::parse(&self.datagram).ok();
        let received = parsed.is_some();
        if received {
            self.packet = parsed;
        }
        Ok(received)
    }

    /// Numeric compatibility replay. Typed dispatch invokes `receive_native` and each requested getter.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let received = self.receive_native(telemetry)?;
        self.write_channels(telemetry)?;
        Ok(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;

    /// A main packet.
    fn packet(flags: u8, speed: f32, rpm: i32) -> Vec<u8> {
        let mut d = vec![0u8; PACKET_MAIN_LEN];
        d[0..4].copy_from_slice(&SIGNATURE.to_le_bytes());
        d[4] = PACKET_MAIN;
        d[layout::STATUS_FLAGS] = flags;
        d[layout::CAR_NAME..layout::CAR_NAME + 5].copy_from_slice(b"Rocky");
        d[layout::DRIVELINE_GEAR] = 3;
        let mut put = |o: usize, v: f32| d[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(layout::DRIVELINE_SPEED, speed);
        put(layout::QUATERNION + 4, 0.3);
        put(layout::QUATERNION + 12, 1.0);
        put(layout::VELOCITY_LOCAL, 0.25);
        put(layout::ACCELERATION_LOCAL + 4, -2.0);
        put(layout::TIRES + RL * layout::TIRE_LEN + layout::TIRE_SUSPENSION_DISPLACEMENT, 0.05);
        d[layout::ENGINE_RPM..layout::ENGINE_RPM + 4].copy_from_slice(&rpm.to_le_bytes());
        d[layout::ENGINE_RPM_MAX..layout::ENGINE_RPM_MAX + 4].copy_from_slice(&9000i32.to_le_bytes());
        d[layout::TIRES + FR * layout::TIRE_LEN + layout::TIRE_SURFACE_TYPE] = 8;
        d
    }

    fn source() -> (Wreckfest2Source, FakeNetwork) {
        let net = FakeNetwork::default();
        (Wreckfest2Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), true, Box::new(net.clone())), net)
    }

    #[test]
    fn native_cached_port_failures_keep_packet_history_and_allow_retry() {
        use super::super::support::net::Datagrams;
        use std::io;
        use std::sync::Arc;

        struct Staged(Arc<Mutex<Vec<ImportedPort>>>, FakeNetwork);
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native import must reach bind_imported")
            }

            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(*endpoint.bind.ip(), std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(endpoint.timeout, Some(RECEIVE_TIMEOUT));
                assert!(!endpoint.broadcast);
                assert!(!endpoint.reuse_address);
                let step = {
                    let mut calls = self.0.lock().unwrap();
                    calls.push(port);
                    calls.len()
                };
                if step == 1 {
                    return Err(super::super::socket::socket_creation_error(io::Error::other("creation denied")));
                }
                let port = port.for_bind()?;
                match step {
                    2 => Err(io::Error::other("bind denied")),
                    3 => Err(io::Error::other("settimeout denied")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port);
                        self.1.bind(&endpoint)
                    }
                }
            }
        }

        let data = packet(0, 10.0, 2400);
        let previous = MainPacket::parse(&data).unwrap();
        let ready = FakeNetwork::default();
        ready.push_timeout(6001);
        ready.push(6001, &packet(0, 20.0, 4800));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = Wreckfest2Source::with_imported_network(
            ImportedPort::Valid(6001),
            true,
            Box::new(Staged(calls.clone(), ready)),
        );
        source.datagram = data.clone();
        source.packet = Some(previous.clone());
        source.rpm_history.lock().unwrap().extend([1200, 2400]);
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, data);
            assert_eq!(source.packet, Some(previous.clone()));
            assert_eq!(*source.rpm_history.lock().unwrap(), [1200, 2400]);
        }
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(*source.rpm_history.lock().unwrap(), [1200, 2400], "receiveData does not call getters");
        assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Float(20.0)));
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = Wreckfest2Source::with_imported_network(
            ImportedPort::OutOfRange,
            false,
            Box::new(Staged(calls.clone(), FakeNetwork::default())),
        );
        assert!(!source.is_running().unwrap(), "creation exception takes precedence over an out-of-range bind");
        assert!(!source.is_running().unwrap(), "the successful creation reaches the invalid bind argument");
        assert_eq!(*calls.lock().unwrap(), [ImportedPort::OutOfRange; 2]);
        assert!(source.packet.is_none());
        assert!(source.rpm_history.lock().unwrap().is_empty());
    }

    #[test]
    fn native_received_invalid_header_replaces_data_without_clearing_the_parsed_packet() {
        let net = FakeNetwork::default();
        let mut source =
            Wreckfest2Source::with_imported_network(ImportedPort::Valid(6001), true, Box::new(net.clone()));
        net.push(6001, &packet(0, 10.0, 2400));
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        let previous = source.packet.clone();

        let short = vec![0; 8];
        net.push(6001, &short);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.datagram, short, "checkStatus stores the datagram before checking its header");
        assert!(!source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.packet, previous);

        let mut wrong_type = packet(0, 30.0, 7200);
        wrong_type[4] = 2;
        net.push(6001, &wrong_type);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.datagram, wrong_type);
        assert_eq!(source.packet, previous, "checkStatus does not parse or reset PacketMain");
        net.push(6001, &packet(0, 40.0, 8000));
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Float(40.0)));
    }

    #[test]
    fn native_receive_preserves_the_rpm_history_until_a_getter_runs() {
        let (mut s, _) = source();
        s.datagram = packet(0, 0.0, 900);
        assert!(s.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(s.rpm_history.lock().unwrap().len(), 0, "receiveData parses without invoking getChannel");
        s.packet.as_mut().unwrap().status_flags = REPLAY;
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        s.set_pause_when_not_playing(false);
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(900)));
        s.set_pause_when_not_playing(true);
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(s.rpm_history.lock().unwrap().len(), 1, "blocked getters do not add history samples");
    }

    #[test]
    fn native_pause_setting_failure_returns_zero_before_rpm_history() {
        use std::sync::Arc;
        let (mut source, _) = source();
        source.datagram = packet(0, 12.0, 900);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        source.set_pause_reader(Arc::new(|| None));
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert!(source.rpm_history.lock().unwrap().is_empty());
    }

    #[test]
    fn native_pause_setting_is_current_for_each_actual_getter() {
        use std::sync::Arc;
        let (mut source, _) = source();
        source.datagram = packet(REPLAY, 12.0, 900);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        let current = Arc::new(Mutex::new(Some(LegacyValue::from("no"))));
        let read = current.clone();
        source.set_pause_reader(Arc::new(move || read.lock().unwrap().clone()));
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(900)));
        *current.lock().unwrap() = Some(LegacyValue::from("yes"));
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(source.rpm_history.lock().unwrap().len(), 1);
    }

    #[test]
    fn native_pause_setting_short_circuits_only_missing_packet_or_paused() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let (mut source, _) = source();
        let current = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        let read = current.clone();
        let counted = calls.clone();
        source.set_pause_reader(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            read.lock().unwrap().clone()
        }));
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        source.datagram = packet(PAUSED, 12.0, 900);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        source.packet.as_mut().unwrap().status_flags = 0;
        for value in [
            None,
            Some(LegacyValue::None),
            Some(LegacyValue::from("yes ")),
            Some(LegacyValue::Int(2)),
            Some(LegacyValue::Float(1.0)),
        ] {
            *current.lock().unwrap() = value;
            let before = calls.load(Ordering::SeqCst);
            assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
            assert!(source.rpm_history.lock().unwrap().is_empty());
        }
        for value in [LegacyValue::from("Y"), LegacyValue::Bool(false), LegacyValue::Int(0)] {
            *current.lock().unwrap() = Some(value);
            assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(900)));
        }
        assert_eq!(source.rpm_history.lock().unwrap().len(), 3);
        source.packet.as_mut().unwrap().status_flags = SPECTATE;
        *current.lock().unwrap() = Some(LegacyValue::Bool(true));
        assert_eq!(source.read_native("unknown"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(source.rpm_history.lock().unwrap().len(), 3);
        *current.lock().unwrap() = Some(LegacyValue::from("no"));
        assert_eq!(source.read_native("unknown"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(source.rpm_history.lock().unwrap().len(), 4, "unblocked unknown getter still appends history");
    }

    #[test]
    fn native_getter_kinds_and_literal_none_name() {
        let (mut s, _) = source();
        assert_eq!(s.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(s.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(0)));
        s.datagram = packet(0, 12.5, 4100);
        s.receive_native(&mut Telemetry::off()).unwrap();
        for (name, value) in [
            ("speed", LegacyValue::Float(12.5)),
            ("rpm", LegacyValue::Int(4100)),
            ("max_rpm", LegacyValue::Int(9000)),
            ("gear", LegacyValue::Int(2)),
            ("longitudinal_velocity", LegacyValue::Float(25.0)),
            ("lateral_acceleration", LegacyValue::Float(-2.0)),
            ("suspension_travel_lr", LegacyValue::Float(f64::from(0.05f32))),
            ("rumble_pitch_rf", LegacyValue::Int(40)),
            ("rumble_pitch_lf", LegacyValue::Int(0)),
            ("yaw_rate", LegacyValue::Int(0)),
            ("track_loc", LegacyValue::Int(0)),
            ("car_name", LegacyValue::Str("Rocky".into())),
        ] {
            assert_eq!(s.read_native(name), NativeRead::Value(value), "{name}");
        }
        let p = s.packet.as_mut().unwrap();
        p.gear = 0;
        p.car_name = "None".into();
        p.rpm = i32::MIN;
        p.speed = f64::NAN;
        assert_eq!(s.read_native("gear"), NativeRead::Value(LegacyValue::Int(-1)));
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(i64::from(i32::MIN))));
        assert_eq!(s.read_native("car_name"), NativeRead::Value(LegacyValue::Str(String::new())));
        assert!(matches!(s.read_native("speed"), NativeRead::Value(LegacyValue::Float(n)) if n.is_nan()));
        s.packet.as_mut().unwrap().car_name = " None ".into();
        assert_eq!(s.read_native("car_name"), NativeRead::Value(LegacyValue::Str(" None ".into())));
    }

    #[test]
    fn actual_getters_advance_history_including_unknown_reads_but_pause_does_not() {
        let (mut s, _) = source();
        s.datagram = packet(PAUSED, 0.0, 900);
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert!(s.rpm_history.lock().unwrap().is_empty());
        s.datagram = packet(0, 0.0, 900);
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(900)));
        for _ in 0..RPM_HISTORY - 2 {
            assert_eq!(s.read_native("missing"), NativeRead::Value(LegacyValue::Int(0)));
        }
        assert_eq!(s.rpm_history.lock().unwrap().len(), RPM_HISTORY - 1);
        assert_eq!(s.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
        s.datagram = packet(0, 0.0, 901);
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.read_native("car_name"), NativeRead::Value(LegacyValue::Str("Rocky".into())));
        s.datagram.clear();
        assert!(!s.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Int(901)));
    }

    #[test]
    fn source_adapter_consumes_one_history_sample_per_actual_read() {
        use crate::derive::{Listener, SourceListener};
        let (mut s, _) = source();
        s.datagram = packet(0, 0.0, 900);
        let mut listener = SourceListener::new(s);
        listener.receive();
        for _ in 0..RPM_HISTORY - 1 {
            assert_eq!(listener.read_native("rpm"), NativeRead::Value(LegacyValue::Int(900)));
        }
        assert_eq!(listener.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(listener.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
    }

    fn tick(s: &mut Wreckfest2Source, net: &FakeNetwork, datagram: &[u8]) -> Telemetry {
        net.push(DEFAULT_PORT, datagram);
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        s.poll(&mut t).unwrap();
        t
    }

    #[test]
    fn layout_sizes() {
        // Layout: header 14 bytes, the car block at 350, the main packet 1218 bytes
        assert_eq!(layout::TIRES + 4 * layout::TIRE_LEN + 14, 882);
        assert_eq!(layout::ENGINE_RPM, 435);
    }

    #[test]
    fn check_status_needs_signature_and_main_type() {
        // Liveness: the header's signature and packet type
        let (mut s, net) = source();
        let mut other = packet(0, 10.0, 3000);
        other[4] = 2;
        net.push(DEFAULT_PORT, &other);
        assert!(!s.is_running().unwrap());
        net.push(DEFAULT_PORT, &[0u8; 8]);
        assert!(!s.is_running().unwrap());
        assert_eq!(net.bound()[0].bind, "127.0.0.1:23123".parse().unwrap());
    }

    #[test]
    fn channels_while_driving() {
        // Channel values
        let (mut s, net) = source();
        let t = tick(&mut s, &net, &packet(0, 12.5, 4100));
        assert_eq!((t.speed, t.rpm, t.max_rpm, t.gear), (12.5, 4100.0, 9000.0, 2.0));
        assert_eq!(t.vehicle, "Rocky");
        assert_eq!(t.longitudinal_velocity, 25.0);
        assert_eq!(t.lateral_acceleration, -2.0);
        assert_eq!(t.suspension_travel_lr, f64::from(0.05f32));
        assert_eq!((t.rumble_pitch_rf, t.rumble_pitch_lf), (40.0, 0.0));
        assert_eq!(t.yaw, quaternion::yaw(0.0, f64::from(0.3f32), 0.0, 1.0));
        // Roll and pitch are negated, yaw is not
        assert_eq!(t.roll, -quaternion::roll(0.0, f64::from(0.3f32), 0.0, 1.0));
        assert_eq!(t.pitch, -quaternion::pitch(0.0, f64::from(0.3f32), 0.0, 1.0));
    }

    #[test]
    fn roll_and_pitch_are_negated_yaw_is_not() {
        // Roll and pitch are the negated quaternion angles;
        // yaw is the quaternion angle as is
        let (mut s, net) = source();
        let mut d = packet(0, 12.5, 4100);
        d[layout::QUATERNION..layout::QUATERNION + 4].copy_from_slice(&0.1f32.to_le_bytes());
        d[layout::QUATERNION + 8..layout::QUATERNION + 12].copy_from_slice(&0.2f32.to_le_bytes());
        let t = tick(&mut s, &net, &d);
        let (x, y, z, w) = (f64::from(0.1f32), f64::from(0.3f32), f64::from(0.2f32), 1.0);
        assert!(quaternion::roll(x, y, z, w) > 0.0 && quaternion::pitch(x, y, z, w) > 0.0);
        assert_eq!(t.roll, -quaternion::roll(x, y, z, w));
        assert_eq!(t.pitch, -quaternion::pitch(x, y, z, w));
        assert_eq!(t.yaw, quaternion::yaw(x, y, z, w));
    }

    #[test]
    fn pause_and_replay_gating() {
        // Blocking game states
        let (mut s, net) = source();
        assert_eq!(tick(&mut s, &net, &packet(PAUSED, 12.5, 4100)).rpm, 0.0);
        assert_eq!(tick(&mut s, &net, &packet(REPLAY, 12.5, 4100)).rpm, 0.0);
        let net = FakeNetwork::default();
        let mut s =
            Wreckfest2Source::with_network(ListenerSettings::with_port(DEFAULT_PORT), false, Box::new(net.clone()));
        assert_eq!(tick(&mut s, &net, &packet(SPECTATE, 12.5, 4100)).rpm, 4100.0);
        assert_eq!(tick(&mut s, &net, &packet(PAUSED, 12.5, 4100)).rpm, 0.0);
    }

    /// The registry's auto-loaded channels, the middle of every tick's read sequence.
    fn registry_reads() -> Vec<String> {
        CHANNEL_SPECS.iter().filter(|spec| spec.auto_load).map(|spec| spec.name.to_string()).collect()
    }

    fn sequence(head: &[&str], tail: &[&str]) -> Vec<String> {
        let mut calls: Vec<String> = head.iter().map(|c| c.to_string()).collect();
        calls.extend(registry_reads());
        calls.extend(tail.iter().map(|c| c.to_string()));
        calls
    }

    #[test]
    fn default_settings_read_sequence() {
        // One dispatcher tick with dams_front_tl_mode 0 and angle-type pitch/roll
        let (mut s, net) = source();
        tick(&mut s, &net, &packet(0, 12.5, 4100));
        let head = [
            "car_name",
            "track_loc",
            "speed",
            "speed",
            "lateral_velocity",
            "longitudinal_velocity",
            "lateral_velocity",
            "speed",
        ];
        assert_eq!(s.calls, sequence(&head, &["yaw", "yaw", "pitch", "roll"]));
    }

    #[test]
    fn dams_mode_2_and_rate_types_change_the_read_sequence() {
        // Mode 2: front reads steering_wheel_angle and yaw_rate, rear reads yaw_rate instead of
        // lateral_velocity; rate-type pitch and roll read pitch_rate and roll_rate
        let (mut s, net) = source();
        s.set_dispatcher_reads(DispatcherReads { dams_front_tl_mode: 2, pitch_angle: false, roll_angle: false });
        let mut t = Telemetry::off();
        t.pitch = 5.0;
        net.push(DEFAULT_PORT, &packet(0, 12.5, 4100));
        assert!(s.is_running().unwrap());
        s.poll(&mut t).unwrap();
        let head = [
            "car_name",
            "track_loc",
            "speed",
            "speed",
            "lateral_velocity",
            "longitudinal_velocity",
            "steering_wheel_angle",
            "yaw_rate",
            "yaw_rate",
            "speed",
        ];
        assert_eq!(s.calls, sequence(&head, &["yaw", "yaw", "pitch_rate", "roll_rate"]));
        assert_eq!(t.pitch, 0.0, "pitch_rate is the dispatcher's, not the listener's pitch");
        assert_eq!(t.lateral_velocity, 0.0, "the first lateral_velocity read");

        // At a standstill (speed 0) mode 2 skips the yaw_rate reads; without a longitudinal
        // velocity it skips the steering read too.
        s.calls.clear();
        let mut standstill = packet(0, 0.0, 4100);
        standstill[layout::VELOCITY_LOCAL..layout::VELOCITY_LOCAL + 4].copy_from_slice(&0.0f32.to_le_bytes());
        tick(&mut s, &net, &standstill);
        let head = ["car_name", "track_loc", "speed", "speed", "lateral_velocity", "longitudinal_velocity", "speed"];
        assert_eq!(s.calls, sequence(&head, &["yaw", "yaw", "pitch_rate", "roll_rate"]));
    }

    #[test]
    fn an_unreadable_speed_stops_after_get_max_speed() {
        // A NaN speed only logs at the first test; the top-speed update's speed read ends the dispatcher
        let (mut s, net) = source();
        net.push(DEFAULT_PORT, &packet(0, f32::NAN, 4100));
        assert!(s.is_running().unwrap());
        let before = Telemetry { rpm: 1234.0, speed: 7.0, location: "x".into(), ..Telemetry::off() };
        let mut t = before.clone();
        crate::udp_racing::unhandled::assert_ends(s.poll(&mut t));
        let head = ["car_name", "track_loc", "speed", "lateral_velocity", "longitudinal_velocity", "lateral_velocity"];
        let mut expected: Vec<String> = head.iter().map(|c| c.to_string()).collect();
        expected.push("speed".into());
        assert_eq!(s.calls, expected);
        let lateral = f64::from(0.0f32) * 100.0;
        assert_eq!(
            t,
            Telemetry { vehicle: "Rocky".into(), location: String::new(), lateral_velocity: lateral, ..before }
        );
    }

    #[test]
    fn frozen_stream_detection() {
        // Frozen stream: 50 equal rpm samples at zero speed read 0
        let (mut s, net) = source();
        // First tick at a standstill: reads 1..49 see a short history, later reads are stuck.
        let t = tick(&mut s, &net, &packet(0, 0.0, 900));
        assert_eq!(t.rpm, 900.0, "an early read of the tick");
        assert_eq!(t.yaw, 0.0, "the 50th and later reads");
        // Next tick with the same rpm: stuck from the first read on.
        let t = tick(&mut s, &net, &packet(0, 0.0, 900));
        assert_eq!(t.lateral_acceleration, 0.0);
        assert_eq!(t.vehicle, "");
        // Moving again: never stuck.
        let t = tick(&mut s, &net, &packet(0, 3.0, 900));
        assert_eq!(t.rpm, 900.0);
        assert_ne!(t.yaw, 0.0);
    }
}

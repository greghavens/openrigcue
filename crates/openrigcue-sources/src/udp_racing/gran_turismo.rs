//! Gran Turismo 7 and Gran Turismo Sport on PlayStation (listeners `gt7` and `gtsport`).
//!
//! The console only streams while it receives a heartbeat: every liveness test binds a fresh socket, sends
//! `b"A"` to the PlayStation and waits up to 3 s for one encrypted 296-byte "packet A".

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::forward::Forwarder;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::{publish_speed_unreadable, zero_listener_channels};
use super::support::clock::{Clock, SystemClock};
use super::support::net::{Endpoint, Network, SystemNetwork};
use super::support::pymath::{degrees, rad_to_deg, trunc};
use super::support::resolver::resolve_ipv4_host;
use super::support::salsa20;
use super::support::wire::{Fields, Short};
use super::unhandled::DispatcherEnds;
use crate::{NativeRead, Source, SourceError};

/// Size of the "packet A" telemetry datagram.
const PACKET_LEN: usize = 296;
/// The magic word of a decrypted packet: `b"0S7G"` little-endian.
const MAGIC: i32 = 0x4737_5330;
/// XOR mask applied to the nonce word.
const NONCE_MASK: u32 = 0xDEAD_BEAF;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(3);
const GRAVITY: f64 = 9.81;

pub type NativeConsoleReader = std::sync::Arc<dyn Fn() -> Option<LegacyValue> + Send + Sync>;

/// Which game the listener serves; the two differ only in ports and key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GranTurismo {
    Gt7,
    Sport,
}

impl GranTurismo {
    /// Port the console's telemetry reaches (default of `NETWORK_PORT.gt7` / `.gtsport`).
    pub fn default_bind_port(self) -> u16 {
        match self {
            GranTurismo::Gt7 => 33740,
            GranTurismo::Sport => 33340,
        }
    }

    /// The console's heartbeat port.
    fn heartbeat_port(self) -> u16 {
        match self {
            GranTurismo::Gt7 => 33739,
            GranTurismo::Sport => 33339,
        }
    }

    fn key(self) -> &'static [u8; 32] {
        match self {
            GranTurismo::Gt7 => b"Simulator Interface Packet GT7 v",
            GranTurismo::Sport => b"Simulator Interface Packet ver 0",
        }
    }

    fn id(self) -> &'static str {
        match self {
            GranTurismo::Gt7 => "gt7",
            GranTurismo::Sport => "gtsport",
        }
    }
}

/// User settings the listener reads.
#[derive(Debug, Clone, PartialEq)]
pub struct GranTurismoSettings {
    /// `NETWORK_PORT.gt7` / `NETWORK_PORT.gtsport`.
    pub bind_port: u16,
    /// `APP.playstation_ip` when the listener is built; empty disables the listener. The listener reads the
    /// current value on every call ([`GranTurismoSource::set_playstation_ip`]).
    pub playstation_ip: String,
}

impl GranTurismoSettings {
    pub fn new(game: GranTurismo, playstation_ip: &str) -> Self {
        GranTurismoSettings { bind_port: game.default_bind_port(), playstation_ip: playstation_ip.to_string() }
    }
}

/// The decoded fields of a "packet A" that the listener uses.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GtPacket {
    magic: i32,
    world_velocity: [f64; 3],
    /// `rotation_x/y/z` and `northorientation`: a quaternion with the scalar last.
    rotation: [f64; 4],
    rpm: f64,
    speed: f64,
    min_alert_rpm: i16,
    flags: i16,
    suggested_gear_gear: u8,
}

impl GtPacket {
    /// Decodes the fields in use from a 296-byte little-endian packet.
    fn parse(plain: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(plain);
        Ok(GtPacket {
            magic: f.i32(0x00)?,
            world_velocity: [f.f32(0x10)?, f.f32(0x14)?, f.f32(0x18)?],
            rotation: [f.f32(0x1C)?, f.f32(0x20)?, f.f32(0x24)?, f.f32(0x28)?],
            rpm: f.f32(0x3C)?,
            speed: f.f32(0x4C)?,
            min_alert_rpm: f.i16(0x88)?,
            flags: f.i16(0x8E)?,
            suggested_gear_gear: f.u8(0x90)?,
        })
    }

    fn on_track(&self) -> bool {
        self.flags & 1 != 0
    }

    fn paused(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// Decrypts a datagram: the Salsa20 nonce is the little-endian word
/// at 0x40 XOR `0xDEADBEAF`, followed by the word itself.
fn decrypt(key: &[u8; 32], data: &[u8]) -> Result<Vec<u8>, Short> {
    let iv1 = Fields::new(data).u32(0x40)?;
    let mut nonce = [0u8; 8];
    nonce[..4].copy_from_slice(&(iv1 ^ NONCE_MASK).to_le_bytes());
    nonce[4..].copy_from_slice(&iv1.to_le_bytes());
    Ok(salsa20::xor(key, &nonce, data))
}

type Vec3 = [f64; 3];

/// The cross product.
fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: Vec3, s: f64) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// Rotates `v` by a quaternion: `v + 2·q⃗ × (q⃗ × v + w·v)` for `q = (x, y, z, w)`.
fn rotate(v: Vec3, q: [f64; 4]) -> Vec3 {
    let qv = [q[0], q[1], q[2]];
    let p = add(cross(qv, v), scale(v, q[3]));
    add(v, cross(scale(qv, 2.0), p))
}

/// World velocity expressed in the car frame: `V` rotated by the conjugate of `Q`.
/// Index 0 is lateral, 1 vertical, 2 longitudinal.
fn local_velocity(p: &GtPacket) -> Vec3 {
    let q = p.rotation;
    rotate(p.world_velocity, [-q[0], -q[1], -q[2], q[3]])
}

/// Euler angles in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Attitude {
    roll: f64,
    pitch: f64,
    yaw: f64,
}

/// Whether `a` and `b` are close, with a relative tolerance of 1e-9.
fn is_close(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
}

/// Roll, pitch and yaw for `q = (x, y, z, w)`. `None` where the arcsine is outside its domain
/// (a non-unit quaternion), which makes every channel read fail.
fn attitude(q: [f64; 4]) -> Option<Attitude> {
    let p = [q[3], q[2], q[0], q[1]];
    let e = -1.0;
    let sin_pitch = 2.0 * (p[0] * p[2] + e * p[1] * p[3]);
    if sin_pitch.abs() > 1.0 {
        return None;
    }
    let pitch = sin_pitch.asin();
    let (roll, yaw) = if is_close(pitch.abs(), std::f64::consts::PI / 2.0) {
        (p[1].atan2(p[0]), 0.0)
    } else {
        let roll = (2.0 * (p[0] * p[1] - e * p[2] * p[3])).atan2(1.0 - 2.0 * (p[1] * p[1] + p[2] * p[2]));
        let yaw = (2.0 * (p[0] * p[3] - e * p[1] * p[2])).atan2(1.0 - 2.0 * (p[2] * p[2] + p[3] * p[3]));
        (roll, yaw)
    };
    Some(Attitude { roll: rad_to_deg(roll), pitch: rad_to_deg(pitch), yaw: rad_to_deg(yaw) })
}

/// Per-axis finite difference of the car-frame velocity (the previous time and
/// previous velocity of each axis, initialised when the listener is built).
#[derive(Debug, Clone)]
struct Differentiator {
    previous_time: [Duration; 3],
    previous_velocity: Vec3,
}

impl Differentiator {
    fn new(now: Duration) -> Self {
        Differentiator { previous_time: [now; 3], previous_velocity: [0.0; 3] }
    }

    /// The acceleration of one axis: velocity change per
    /// second over the sub-second part of the elapsed time, in g, times `gain`.
    fn acceleration(&mut self, axis: usize, velocity: f64, now: Duration, gain: f64) -> f64 {
        self.native_acceleration(axis, velocity, now, gain).number().expect("numeric acceleration")
    }

    fn native_acceleration(&mut self, axis: usize, velocity: f64, now: Duration, gain: f64) -> LegacyValue {
        // Times have microsecond resolution. The interval used is the normalized subsecond part,
        // including for a negative interval; whole seconds and days do not enter this calculation.
        let micros = (now.subsec_micros() + 1_000_000 - self.previous_time[axis].subsec_micros()) % 1_000_000;
        self.previous_time[axis] = Duration::new(now.as_secs(), now.subsec_micros() * 1000);
        let value = if micros != 0 {
            LegacyValue::Float(
                (velocity - self.previous_velocity[axis]) * 1_000_000.0 / f64::from(micros) / GRAVITY * gain,
            )
        } else {
            LegacyValue::Int(0)
        };
        self.previous_velocity[axis] = velocity;
        value
    }
}

/// The Gran Turismo 7 / Gran Turismo Sport listener.
pub struct GranTurismoSource {
    game: GranTurismo,
    settings: GranTurismoSettings,
    console_reader: Option<NativeConsoleReader>,
    imported_port: Option<ImportedPort>,
    net: Box<dyn Network>,
    clock: Box<dyn Clock>,
    forwarder: Forwarder,
    /// Last datagram the liveness test received.
    datagram: Option<Vec<u8>>,
    /// Last decoded packet, kept even when its magic is wrong.
    packet: Option<GtPacket>,
    differentiator: Mutex<Differentiator>,
}

impl GranTurismoSource {
    pub fn new(game: GranTurismo, settings: GranTurismoSettings) -> Self {
        Self::with_parts(game, settings, Box::new(SystemNetwork), Box::new(SystemClock))
    }

    /// The configured port as an integer, before the socket applies its port range.
    pub fn from_imported_port(game: GranTurismo, settings: GranTurismoSettings, port: ImportedPort) -> Self {
        let mut source = Self::new(game, settings);
        source.imported_port = Some(port);
        source
    }

    fn with_parts(
        game: GranTurismo,
        settings: GranTurismoSettings,
        net: Box<dyn Network>,
        clock: Box<dyn Clock>,
    ) -> Self {
        let forwarder = Forwarder::default();
        // The current time is read separately for x, y and z.
        let mut differentiator = Differentiator::new(clock.now());
        differentiator.previous_time[1] = clock.now();
        differentiator.previous_time[2] = clock.now();
        let differentiator = Mutex::new(differentiator);
        GranTurismoSource {
            game,
            settings,
            console_reader: None,
            imported_port: None,
            net,
            clock,
            forwarder,
            datagram: None,
            packet: None,
            differentiator,
        }
    }

    /// Numeric compatibility setting; production typed calls use the optional current raw reader.
    pub fn set_playstation_ip(&mut self, playstation_ip: &str) {
        playstation_ip.clone_into(&mut self.settings.playstation_ip);
    }

    pub fn set_console_reader(&mut self, reader: NativeConsoleReader) {
        self.console_reader = Some(reader);
    }

    fn enabled(&self) -> bool {
        !self.settings.playstation_ip.is_empty()
    }

    fn console_value(&self) -> Option<LegacyValue> {
        match &self.console_reader {
            Some(reader) => reader(),
            None => Some(LegacyValue::Str(self.settings.playstation_ip.clone())),
        }
    }

    fn native_enabled(&self) -> Option<bool> {
        self.console_value().map(|value| !matches!(value, LegacyValue::Str(text) if text.is_empty()))
    }

    /// The liveness test: heartbeat, then wait for one datagram on a fresh socket; the datagram is
    /// forwarded and kept, and the game counts as running only when it is exactly 296 bytes.
    fn heartbeat_and_receive(&mut self) -> bool {
        self.heartbeat_and_receive_with(crate::printed::line)
    }

    fn heartbeat_and_receive_with(&mut self, mut report: impl FnMut(String)) -> bool {
        let endpoint = Endpoint { broadcast: true, ..Endpoint::any(self.settings.bind_port, RECEIVE_TIMEOUT) };
        let opened = match self.imported_port {
            Some(port) => self.net.bind_imported(&endpoint, port),
            None => self.net.bind(&endpoint),
        };
        let mut socket = match opened {
            Ok(s) => s,
            Err(e) => {
                // Socket creation and SO_BROADCAST fail silently.
                if !socket_creation_failed(&e) {
                    report(format!("socket error {}", crate::printed::os_error(&e)));
                }
                return false;
            }
        };
        // The heartbeat send resolves a host name as well as a dotted address.
        // A missing setting and a setting of another type each fail with their own text.
        let console = match self.console_value() {
            Some(LegacyValue::Str(host)) => resolve_ipv4_host(&host)
                .map(|address| std::net::SocketAddrV4::new(address, self.game.heartbeat_port()).into())
                .map_err(|error| error.to_string()),
            Some(other) => Err(format!("str, bytes or bytearray expected, not {}", other.type_name())),
            None => Err("'playstation_ip'".to_owned()),
        };
        // Prints "error sending a" and the error text.
        let sent = console.and_then(|to| socket.send_to(b"A", to).map_err(|error| crate::printed::os_error(&error)));
        if let Err(text) = sent {
            report(format!("error sending a {text}"));
            return false;
        }
        match socket.receive(PACKET_LEN) {
            Ok((data, _)) => {
                self.forwarder.forward(&data);
                let accepted = data.len() == PACKET_LEN;
                self.datagram = Some(data);
                accepted
            }
            Err(e) => {
                // Prints "recev from error" and the error text: `timed out` while the console is silent.
                report(format!("recev from error {}", crate::printed::os_error(&e)));
                false
            }
        }
    }

    /// The receive step: decrypt the last datagram and keep the packet.
    fn decode(&mut self) -> bool {
        let Some(data) = self.datagram.as_deref() else { return false };
        let Ok(plain) = decrypt(self.game.key(), data) else {
            crate::printed::line("Failed to decrypt data");
            return false;
        };
        if data.len() != PACKET_LEN {
            return false;
        }
        match GtPacket::parse(&plain) {
            Ok(packet) => {
                self.packet = Some(packet);
                packet.magic == MAGIC
            }
            // A decode failure cannot be reached: a 296-byte packet always decodes.
            Err(_) => false,
        }
    }

    /// Writes every channel the dispatcher reads. The packet
    /// (absent before the first one) and the attitude (arcsine outside its domain) are read before any
    /// channel and their failure is not handled, so every read fails; so does a NaN or infinite
    /// speed for the speed read. In both cases the tick ends after `lateral_velocity`
    /// ([`publish_speed_unreadable`]). Where the rpm has no integer value the dispatcher keeps the previous rpm.
    fn write_channels(&mut self, t: &mut Telemetry) -> Result<(), DispatcherEnds> {
        if !self.enabled() {
            zero_listener_channels(t);
            return Ok(());
        }
        let Some(p) = self.packet else {
            return publish_speed_unreadable(t, None, 0.0);
        };
        if !p.on_track() || p.paused() {
            zero_listener_channels(t);
            return Ok(());
        }
        let Some(att) = attitude(p.rotation) else {
            return publish_speed_unreadable(t, None, 0.0);
        };
        let lv = local_velocity(&p);
        let lateral_velocity = if lv[2] == 0.0 { 0.0 } else { degrees((lv[0] / lv[2].abs()).atan()) };
        let Some(speed) = trunc(p.speed) else {
            return publish_speed_unreadable(t, Some(String::new()), lateral_velocity);
        };
        let rpm = t.rpm;
        zero_listener_channels(t);
        t.rpm = trunc(p.rpm).unwrap_or(rpm);
        t.max_rpm = f64::from(p.min_alert_rpm);
        t.gear = f64::from(p.suggested_gear_gear & 0x0F);
        t.speed = speed;
        t.lateral_velocity = lateral_velocity;
        // The dispatcher reads lateral, longitudinal, then vertical acceleration.
        t.lateral_acceleration = self.differentiator.lock().unwrap_or_else(PoisonError::into_inner).acceleration(
            0,
            lv[0],
            self.clock.now(),
            8.0,
        );
        t.longitudinal_acceleration = self.differentiator.lock().unwrap_or_else(PoisonError::into_inner).acceleration(
            2,
            lv[2],
            self.clock.now(),
            -8.0,
        );
        t.vertical_acceleration = self.differentiator.lock().unwrap_or_else(PoisonError::into_inner).acceleration(
            1,
            lv[1],
            self.clock.now(),
            10.0,
        );
        t.yaw = att.yaw;
        t.roll = -att.roll;
        t.pitch = att.pitch;
        Ok(())
    }
}

impl Source for GranTurismoSource {
    fn id(&self) -> &'static str {
        self.game.id()
    }

    fn read_native(&self, name: &str) -> NativeRead {
        use LegacyValue::{Float, Int};
        match self.native_enabled() {
            None => return NativeRead::Raised,
            Some(false) => return NativeRead::Value(Int(0)),
            Some(true) => {}
        }
        let Some(p) = self.packet else { return NativeRead::Raised };
        if !p.on_track() || p.paused() {
            return NativeRead::Value(Int(0));
        }
        let Some(att) = attitude(p.rotation) else { return NativeRead::Raised };
        let lv = local_velocity(&p);
        let value = match name {
            "rpm" => return super::native_int(p.rpm).map_or(NativeRead::Raised, NativeRead::Value),
            "speed" => return super::native_int(p.speed).map_or(NativeRead::Raised, NativeRead::Value),
            "max_rpm" => Int(i64::from(p.min_alert_rpm)),
            "gear" => Int(i64::from(p.suggested_gear_gear & 0x0f)),
            "yaw" => Float(att.yaw),
            "roll" => Float(-att.roll),
            "pitch" => Float(att.pitch),
            "lateral_velocity" => {
                if lv[2] == 0.0 {
                    Int(0)
                } else {
                    Float(degrees((lv[0] / lv[2].abs()).atan()))
                }
            }
            "lateral_acceleration" | "longitudinal_acceleration" | "vertical_acceleration" => {
                let (axis, gain) = match name {
                    "lateral_acceleration" => (0, 8.0),
                    "longitudinal_acceleration" => (2, -8.0),
                    _ => (1, 10.0),
                };
                self.differentiator.lock().unwrap_or_else(PoisonError::into_inner).native_acceleration(
                    axis,
                    lv[axis],
                    self.clock.now(),
                    gain,
                )
            }
            _ => Int(0),
        };
        NativeRead::Value(value)
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let enabled = self
            .native_enabled()
            .ok_or_else(|| DispatcherEnds::at_receive_data("required APP.playstation_ip lookup raised"))?;
        Ok(enabled && self.decode())
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        let enabled =
            self.native_enabled().ok_or_else(|| DispatcherEnds::new("required APP.playstation_ip lookup raised"))?;
        Ok(enabled && self.heartbeat_and_receive())
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
    use crate::udp_racing::forward::test_forwarder;
    use crate::udp_racing::support::clock::fake::FakeClock;
    use crate::udp_racing::support::net::fake::{FakeNetwork, ForwardSink};
    use crate::udp_racing::unhandled::assert_ends;

    /// A plaintext packet A in the layout [`GtPacket::parse`] reads.
    fn plain_packet(flags: i16, rotation: [f32; 4], world_velocity: [f32; 3]) -> Vec<u8> {
        let mut p = vec![0u8; PACKET_LEN];
        p[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        for (i, v) in world_velocity.iter().enumerate() {
            p[0x10 + 4 * i..0x14 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in rotation.iter().enumerate() {
            p[0x1C + 4 * i..0x20 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        p[0x3C..0x40].copy_from_slice(&6543.9f32.to_le_bytes());
        p[0x40..0x44].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        p[0x4C..0x50].copy_from_slice(&41.7f32.to_le_bytes());
        p[0x88..0x8A].copy_from_slice(&7200i16.to_le_bytes());
        p[0x8E..0x90].copy_from_slice(&flags.to_le_bytes());
        p[0x90] = 0x43;
        p
    }

    /// Encrypt like the console: Salsa20 is symmetric, the nonce word at 0x40 travels in clear.
    fn encrypt(game: GranTurismo, plain: &[u8]) -> Vec<u8> {
        let mut cipher = decrypt(game.key(), plain).unwrap();
        cipher[0x40..0x44].copy_from_slice(&plain[0x40..0x44]);
        cipher
    }

    fn source(game: GranTurismo, ip: &str) -> (GranTurismoSource, FakeNetwork, FakeClock) {
        let net = FakeNetwork::default();
        let clock = FakeClock::default();
        let s = GranTurismoSource::with_parts(
            game,
            GranTurismoSettings::new(game, ip),
            Box::new(net.clone()),
            Box::new(clock.clone()),
        );
        (s, net, clock)
    }

    #[test]
    fn native_console_getters_use_the_current_required_setting() {
        use std::sync::Arc;
        for game in [GranTurismo::Gt7, GranTurismo::Sport] {
            let (mut source, _, _) = source(game, "192.168.1.50");
            let current = Arc::new(Mutex::new(Some(LegacyValue::from(""))));
            let read = current.clone();
            source.set_console_reader(Arc::new(move || read.lock().unwrap().clone()));
            assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Int(0)));
            *current.lock().unwrap() = Some(LegacyValue::from("192.168.1.50"));
            assert_eq!(source.read_native("speed"), NativeRead::Raised, "enabled but no decoded packet");
            *current.lock().unwrap() = None;
            assert_eq!(source.read_native("speed"), NativeRead::Raised, "required setting missing");
        }
    }

    #[test]
    fn native_console_missing_setting_ends_before_socket_or_decode() {
        use std::sync::Arc;
        for game in [GranTurismo::Gt7, GranTurismo::Sport] {
            let (mut source, net, _) = source(game, "");
            source.set_console_reader(Arc::new(|| None));
            let error = source.is_running().expect_err("missing required setting precedes disabled fallback");
            assert!(super::super::unhandled::ends_dispatcher(&error));
            assert!(net.bound().is_empty());
            let error = source.receive_native(&mut Telemetry::off()).expect_err("required read precedes decode");
            assert_eq!(super::super::unhandled::failure_point(&error), Some(crate::derive::FailurePoint::ReceiveData));
        }
    }

    #[test]
    fn native_console_heartbeat_rereads_after_setup_and_catches_late_lookup_failure() {
        use std::{
            collections::VecDeque,
            io,
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
        };
        for game in [GranTurismo::Gt7, GranTurismo::Sport] {
            for late in [None, Some(LegacyValue::Int(1)), Some(LegacyValue::from("192.168.1.60"))] {
                let (mut source, net, _) = source(game, "192.168.1.50");
                let values =
                    Arc::new(Mutex::new(VecDeque::from([Some(LegacyValue::from("192.168.1.50")), late.clone()])));
                let calls = Arc::new(AtomicUsize::new(0));
                let counted = calls.clone();
                let read = values.clone();
                source.set_console_reader(Arc::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    read.lock().unwrap().pop_front().expect("unexpected setting read")
                }));
                net.push(game.default_bind_port(), &[0; PACKET_LEN]);
                assert_eq!(source.is_running().unwrap(), matches!(late, Some(LegacyValue::Str(_))));
                assert_eq!(calls.load(Ordering::SeqCst), 2);
                assert_eq!(net.bound().len(), 1, "late lookup is after successful socket setup");
                if matches!(late, Some(LegacyValue::Str(_))) {
                    assert_eq!(
                        net.sent(),
                        [(b"A".to_vec(), format!("192.168.1.60:{}", game.heartbeat_port()).parse().unwrap())]
                    );
                } else {
                    assert!(net.sent().is_empty());
                }
            }
            struct FailBind(Arc<AtomicUsize>);
            impl Network for FailBind {
                fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn super::super::support::net::Datagrams>> {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Err(io::Error::other("setup failed"))
                }
            }
            let bound = Arc::new(AtomicUsize::new(0));
            let mut source = GranTurismoSource::with_parts(
                game,
                GranTurismoSettings::new(game, ""),
                Box::new(FailBind(bound.clone())),
                Box::new(FakeClock::default()),
            );
            let reads = Arc::new(AtomicUsize::new(0));
            let counted = reads.clone();
            source.set_console_reader(Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Some(LegacyValue::from("192.168.1.50"))
            }));
            assert!(!source.is_running().unwrap());
            assert_eq!(bound.load(Ordering::SeqCst), 1);
            assert_eq!(reads.load(Ordering::SeqCst), 1, "failed setup skips the destination lookup");
        }
    }

    #[test]
    fn native_console_empty_setting_skips_transport_and_preserves_decoder_state() {
        use std::sync::Arc;
        for game in [GranTurismo::Gt7, GranTurismo::Sport] {
            let (mut source, net, clock) = source(game, "192.168.1.50");
            let current = Arc::new(Mutex::new(Some(LegacyValue::from(""))));
            let read = current.clone();
            source.set_console_reader(Arc::new(move || read.lock().unwrap().clone()));
            source.datagram = Some(encrypt(game, &plain_packet(1, [0.0; 4], [2.0, 3.0, 4.0])));
            let original = source.datagram.clone();
            assert!(!source.is_running().unwrap());
            assert!(net.bound().is_empty());
            assert!(!source.receive_native(&mut Telemetry::off()).unwrap());
            assert!(source.packet.is_none());
            assert_eq!(source.datagram, original);
            assert_eq!(source.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Int(0)));
            *current.lock().unwrap() = Some(LegacyValue::from("192.168.1.50"));
            assert!(source.receive_native(&mut Telemetry::off()).unwrap());
            assert!(matches!(source.read_native("speed"), NativeRead::Value(LegacyValue::Int(_))));
            *clock.0.lock().unwrap() = Duration::from_millis(20);
            *current.lock().unwrap() = Some(LegacyValue::None);
            assert!(
                matches!(source.read_native("speed"), NativeRead::Value(LegacyValue::Int(_))),
                "present None is not equal to empty text"
            );
            *current.lock().unwrap() = None;
            assert_eq!(source.read_native("lateral_acceleration"), NativeRead::Raised);
        }
    }

    #[test]
    fn native_socket_setup_failures_keep_packets_and_only_late_errors_are_reported() {
        use crate::udp_racing::socket::socket_creation_error;
        use std::io;
        use std::sync::Arc;

        struct Staged {
            calls: Arc<Mutex<Vec<ImportedPort>>>,
            step: usize,
            ready: FakeNetwork,
        }
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn super::super::support::net::Datagrams>> {
                panic!("native source must retain its import-time integer")
            }
            fn bind_imported(
                &mut self,
                endpoint: &Endpoint,
                port: ImportedPort,
            ) -> io::Result<Box<dyn super::super::support::net::Datagrams>> {
                assert!(endpoint.broadcast);
                assert_eq!(endpoint.timeout, Some(RECEIVE_TIMEOUT));
                self.calls.lock().unwrap().push(port);
                self.step += 1;
                match self.step {
                    1 | 2 => Err(socket_creation_error(io::Error::other(if self.step == 1 {
                        "create failed"
                    } else {
                        "broadcast failed"
                    }))),
                    3 => Err(io::Error::other("bind failed")),
                    4 => Err(io::Error::other("timeout failed")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port.for_bind()?);
                        self.ready.bind(&endpoint)
                    }
                }
            }
        }
        for game in [GranTurismo::Gt7, GranTurismo::Sport] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let ready = FakeNetwork::default();
            let new_data = encrypt(game, &plain_packet(1, [0.0, 0.0, 0.0, 1.0], [0.0; 3]));
            ready.push(6001, &new_data);
            let mut listener = GranTurismoSource::with_parts(
                game,
                GranTurismoSettings::new(game, "127.0.0.1"),
                Box::new(Staged { calls: calls.clone(), step: 0, ready: ready.clone() }),
                Box::<FakeClock>::default(),
            );
            listener.imported_port = Some(ImportedPort::Valid(6001));
            listener.datagram = Some(vec![9; PACKET_LEN]);
            let old = GtPacket::parse(&plain_packet(1, [0.0, 0.0, 0.0, 1.0], [0.0; 3])).unwrap();
            listener.packet = Some(old);
            let mut reports = Vec::new();
            for _ in 0..4 {
                assert!(!listener.heartbeat_and_receive_with(|line| reports.push(line)));
                assert_eq!(listener.datagram, Some(vec![9; PACKET_LEN]));
                assert_eq!(listener.packet.as_ref().unwrap().rpm, old.rpm);
                assert!(ready.sent().is_empty());
            }
            assert_eq!(reports, ["socket error bind failed", "socket error timeout failed"]);
            assert!(listener.heartbeat_and_receive_with(|line| reports.push(line)));
            assert_eq!(listener.datagram, Some(new_data));
            assert_eq!(listener.packet.as_ref().unwrap().rpm, old.rpm, "liveness does not decode");
            assert_eq!(*calls.lock().unwrap(), [ImportedPort::Valid(6001); 5]);
            assert_eq!(
                ready.sent(),
                [(b"A".to_vec(), format!("127.0.0.1:{}", game.heartbeat_port()).parse().unwrap())]
            );
        }
    }

    #[test]
    fn native_out_of_range_port_stays_dormant_until_playstation_ip_is_enabled() {
        use std::io;
        use std::sync::Arc;
        struct OutOfRange(Arc<Mutex<usize>>);
        impl Network for OutOfRange {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn super::super::support::net::Datagrams>> {
                panic!("native source must use its imported port")
            }
            fn bind_imported(
                &mut self,
                _: &Endpoint,
                port: ImportedPort,
            ) -> io::Result<Box<dyn super::super::support::net::Datagrams>> {
                *self.0.lock().unwrap() += 1;
                assert_eq!(port, ImportedPort::OutOfRange);
                Err(port.for_bind().unwrap_err())
            }
        }
        for game in [GranTurismo::Gt7, GranTurismo::Sport] {
            let attempts = Arc::new(Mutex::new(0));
            let mut listener = GranTurismoSource::from_imported_port(
                game,
                GranTurismoSettings::new(game, ""),
                ImportedPort::OutOfRange,
            );
            listener.net = Box::new(OutOfRange(attempts.clone()));
            assert!(!listener.is_running().unwrap());
            assert_eq!(*attempts.lock().unwrap(), 0);
            listener.set_playstation_ip("127.0.0.1");
            let mut reports = Vec::new();
            assert!(!listener.heartbeat_and_receive_with(|line| reports.push(line)));
            assert_eq!(*attempts.lock().unwrap(), 1);
            assert_eq!(reports, ["socket error bind(): port must be 0-65535."]);
        }
    }

    #[test]
    fn native_receive_does_not_replay_acceleration_getters() {
        let (mut s, _, clock) = source(GranTurismo::Gt7, "192.168.1.50");
        s.datagram = Some(encrypt(GranTurismo::Gt7, &plain_packet(1, [0.0, 0.0, 0.0, 1.0], [9.81, 0.0, 0.0])));
        *clock.0.lock().unwrap() = Duration::from_millis(500);
        assert!(s.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(s.differentiator.lock().unwrap().previous_time, [Duration::ZERO; 3]);
    }

    #[test]
    fn native_elapsed_time_quantizes_each_datetime_before_subtraction() {
        let mut d = Differentiator::new(Duration::from_nanos(999));
        let value = d.acceleration(0, 9.81, Duration::from_nanos(1001), 1.0);
        assert!((value - 1_000_000.0).abs() < 1e-6, "one microsecond between represented datetime values: {value}");
    }

    #[test]
    fn native_elapsed_time_retains_the_wrapped_microseconds_of_a_backward_clock() {
        let mut d = Differentiator::new(Duration::from_millis(500));
        let value = d.acceleration(0, 9.81, Duration::from_millis(250), 8.0);
        assert!((value - 8.0 / 0.75).abs() < 1e-12, "negative timedelta has 750000 microseconds: {value}");
    }

    #[test]
    fn native_kinds_global_gates_and_individual_integer_failures() {
        use LegacyValue::{Float, Int};
        for game in [GranTurismo::Gt7, GranTurismo::Sport] {
            let (mut s, _, _) = source(game, "192.168.1.50");
            assert_eq!(s.read_native("speed"), NativeRead::Raised);
            s.set_playstation_ip("");
            assert_eq!(s.read_native("speed"), NativeRead::Value(Int(0)));
            s.set_playstation_ip("192.168.1.50");
            s.packet = Some(GtPacket::parse(&plain_packet(1, [0.0, 0.0, 0.0, 1.0], [0.0; 3])).unwrap());
            for (name, value) in [
                ("rpm", Int(6543)),
                ("max_rpm", Int(7200)),
                ("gear", Int(3)),
                ("speed", Int(41)),
                ("yaw", Float(0.0)),
                ("pitch", Float(0.0)),
                ("roll", Float(-0.0)),
                ("lateral_velocity", Int(0)),
                ("lateral_acceleration", Int(0)),
                ("missing", Int(0)),
            ] {
                assert_eq!(s.read_native(name), NativeRead::Value(value), "{name}");
            }
            s.packet.as_mut().unwrap().rpm = f64::NAN;
            assert_eq!(s.read_native("rpm"), NativeRead::Raised);
            assert_eq!(s.read_native("speed"), NativeRead::Value(Int(41)));
            s.packet.as_mut().unwrap().rpm = 6500.0;
            s.packet.as_mut().unwrap().speed = f64::INFINITY;
            assert_eq!(s.read_native("speed"), NativeRead::Raised);
            assert_eq!(s.read_native("rpm"), NativeRead::Value(Int(6500)));
            s.packet = Some(GtPacket::parse(&plain_packet(3, [1.0, 0.0, 0.0, 1.0], [0.0; 3])).unwrap());
            assert_eq!(s.read_native("max_rpm"), NativeRead::Value(Int(0)), "pause precedes quaternion conversion");
            s.packet = Some(GtPacket::parse(&plain_packet(1, [1.0, 0.0, 0.0, 1.0], [0.0; 3])).unwrap());
            assert_eq!(s.read_native("max_rpm"), NativeRead::Raised);
            assert_eq!(s.read_native("missing"), NativeRead::Raised);
        }
    }

    #[test]
    fn native_acceleration_reads_update_only_their_axis_and_paused_reads_keep_history() {
        let (mut s, _, clock) = source(GranTurismo::Gt7, "192.168.1.50");
        s.packet = Some(GtPacket::parse(&plain_packet(1, [0.0, 0.0, 0.0, 1.0], [9.81, 0.0, 0.0])).unwrap());
        *clock.0.lock().unwrap() = Duration::from_millis(500);
        assert_eq!(s.read_native("speed"), NativeRead::Value(LegacyValue::Int(41)));
        let NativeRead::Value(LegacyValue::Float(first)) = s.read_native("lateral_acceleration") else {
            panic!("float acceleration")
        };
        assert!((first - 16.0).abs() < 1e-5);
        assert_eq!(
            s.differentiator.lock().unwrap().previous_time,
            [Duration::from_millis(500), Duration::ZERO, Duration::ZERO]
        );
        assert_eq!(s.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Int(0)));
        *clock.0.lock().unwrap() = Duration::from_millis(750);
        s.packet = Some(GtPacket::parse(&plain_packet(1, [0.0, 0.0, 0.0, 1.0], [19.62, 0.0, 0.0])).unwrap());
        let NativeRead::Value(LegacyValue::Float(second)) = s.read_native("lateral_acceleration") else {
            panic!("float acceleration")
        };
        assert!((second - 32.0).abs() < 1e-5);
        *clock.0.lock().unwrap() = Duration::from_millis(1000);
        s.packet = Some(GtPacket::parse(&plain_packet(3, [0.0, 0.0, 0.0, 1.0], [29.43, 0.0, 0.0])).unwrap());
        assert_eq!(s.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(s.differentiator.lock().unwrap().previous_time[0], Duration::from_millis(750));
        *clock.0.lock().unwrap() = Duration::from_millis(1250);
        s.packet = Some(GtPacket::parse(&plain_packet(1, [0.0, 0.0, 0.0, 1.0], [29.43, 0.0, 0.0])).unwrap());
        let NativeRead::Value(LegacyValue::Float(third)) = s.read_native("lateral_acceleration") else {
            panic!("float acceleration")
        };
        assert!((third - 16.0).abs() < 1e-5);
    }

    #[test]
    fn native_initial_timestamps_and_axis_reads_use_separate_clock_calls() {
        struct ScriptedClock(Mutex<std::collections::VecDeque<Duration>>);
        impl Clock for ScriptedClock {
            fn now(&self) -> Duration {
                self.0.lock().unwrap().pop_front().expect("unexpected datetime.now call")
            }
        }
        let clock = ScriptedClock(Mutex::new([100, 200, 300, 500, 600, 700].map(Duration::from_micros).into()));
        let mut s = GranTurismoSource::with_parts(
            GranTurismo::Gt7,
            GranTurismoSettings::new(GranTurismo::Gt7, "192.168.1.50"),
            Box::<FakeNetwork>::default(),
            Box::new(clock),
        );
        s.packet = Some(GtPacket::parse(&plain_packet(1, [0.0, 0.0, 0.0, 1.0], [9.81; 3])).unwrap());
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.read_native("speed"), NativeRead::Value(LegacyValue::Int(41)));
        let velocity = f64::from(9.81f32);
        for (name, micros, gain) in [
            ("lateral_acceleration", 400.0, 8.0),
            ("longitudinal_acceleration", 300.0, -8.0),
            ("vertical_acceleration", 500.0, 10.0),
        ] {
            let NativeRead::Value(LegacyValue::Float(value)) = s.read_native(name) else {
                panic!("float acceleration")
            };
            let expected = velocity * 1_000_000.0 / micros / GRAVITY * gain;
            assert!((value - expected).abs() < 1e-8, "{name}: {value}, expected {expected}");
        }
    }

    #[test]
    fn source_adapter_does_not_consume_acceleration_during_receive_or_repeat_the_getter() {
        use crate::derive::{Listener, SourceListener};
        let (mut s, _, clock) = source(GranTurismo::Gt7, "192.168.1.50");
        s.datagram = Some(encrypt(GranTurismo::Gt7, &plain_packet(1, [0.0, 0.0, 0.0, 1.0], [9.81, 0.0, 0.0])));
        *clock.0.lock().unwrap() = Duration::from_millis(500);
        let mut listener = SourceListener::new(s);
        listener.receive();
        *clock.0.lock().unwrap() = Duration::from_millis(625);
        let NativeRead::Value(LegacyValue::Float(value)) = listener.read_native("lateral_acceleration") else {
            panic!("float acceleration")
        };
        assert!((value - 12.8).abs() < 1e-5);
        *clock.0.lock().unwrap() = Duration::from_millis(750);
        assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Float(0.0)));
        assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Int(0)));
    }

    #[test]
    fn check_status_sends_heartbeat_and_needs_a_datagram() {
        // GT7 liveness test
        let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
        assert!(!s.is_running().unwrap(), "no datagram within the timeout");
        net.push(33740, &[0u8; PACKET_LEN]);
        assert!(s.is_running().unwrap());
        let bound = net.bound();
        assert_eq!(bound[0].bind, "0.0.0.0:33740".parse().unwrap());
        assert!(bound[0].broadcast);
        assert_eq!(bound[0].timeout, Some(Duration::from_secs(3)));
        assert_eq!(net.sent()[0], (b"A".to_vec(), "192.168.1.50:33739".parse().unwrap()));
    }

    #[test]
    fn the_heartbeat_goes_to_a_host_name_too() {
        // GT7 liveness test: the heartbeat to port 33739 resolves a host name.
        let (mut s, net, _) = source(GranTurismo::Gt7, "localhost");
        net.push(33740, &[0u8; PACKET_LEN]);
        assert!(s.is_running().unwrap());
        assert_eq!(net.sent()[0], (b"A".to_vec(), "127.0.0.1:33739".parse().unwrap()));
        let (mut s, net, _) = source(GranTurismo::Gt7, "<broadcast>");
        net.push(33740, &[0u8; PACKET_LEN]);
        assert!(s.is_running().unwrap());
        assert_eq!(net.sent()[0], (b"A".to_vec(), "255.255.255.255:33739".parse().unwrap()));
        let (mut s, net, _) = source(GranTurismo::Gt7, "no such host.invalid");
        net.push(33740, &[0u8; PACKET_LEN]);
        assert!(!s.is_running().unwrap(), "a name getaddrinfo cannot resolve is 'error sending a'");
        assert!(net.sent().is_empty());
    }

    #[test]
    fn empty_playstation_ip_disables_everything() {
        // An empty playstation_ip: the liveness test answers false and channels read 0
        let (mut s, net, _) = source(GranTurismo::Gt7, "");
        net.push(33740, &[0u8; 10]);
        assert!(!s.is_running().unwrap());
        assert!(net.bound().is_empty());
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.gear, 0.0);
    }

    #[test]
    fn gt_sport_uses_its_own_ports_and_key() {
        // GT Sport: heartbeat port 33339, key 'Simulator Interface Packet ver 0'
        let (mut s, net, _) = source(GranTurismo::Sport, "10.0.0.2");
        let plain = plain_packet(1, [0.0, 0.0, 0.0, 1.0], [0.0; 3]);
        net.push(33340, &encrypt(GranTurismo::Sport, &plain));
        assert!(s.is_running().unwrap());
        assert_eq!(net.sent()[0].1, "10.0.0.2:33339".parse().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 6543.0);
    }

    #[test]
    fn decodes_channels_from_a_decrypted_packet() {
        // Receive step and channels: rpm/max_rpm/gear/speed and the identity attitude
        let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
        net.push(33740, &encrypt(GranTurismo::Gt7, &plain_packet(1, [0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 20.0])));
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 6543.0);
        assert_eq!(t.max_rpm, 7200.0);
        assert_eq!(t.gear, 3.0, "low nibble of suggestedgear_gear");
        assert_eq!(t.speed, 41.0);
        assert_eq!((t.roll, t.pitch, t.yaw), (-0.0, 0.0, 0.0));
        assert_eq!(t.lateral_velocity, 0.0, "atan(0 / 20)");
        assert_eq!(t.steering_wheel_angle, 0.0, "every other channel reads 0");
    }

    /// The listener's messages reach standard output: a console that sends nothing, then a datagram too
    /// short to decrypt.
    #[test]
    fn prints_reach_standard_output() {
        let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
        crate::printed::take();
        assert!(!s.is_running().unwrap());
        assert_eq!(crate::printed::take(), ["recev from error timed out"], "print(text, str(e))");
        net.push(33740, &[1, 2, 3]);
        assert!(!s.is_running().unwrap());
        assert!(crate::printed::take().is_empty());
        let mut t = Telemetry::off();
        // (The later speed read fails, as before; the message comes first.)
        let _ = s.poll(&mut t);
        assert_eq!(crate::printed::take(), ["Failed to decrypt data"]);
    }

    #[test]
    fn wrong_magic_is_rejected_but_kept() {
        // Receive step: the packet is kept before the magic test
        let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
        let mut plain = plain_packet(1, [0.0, 0.0, 0.0, 1.0], [0.0; 3]);
        plain[0] = 0;
        net.push(33740, &encrypt(GranTurismo::Gt7, &plain));
        s.is_running().unwrap();
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 6543.0);
    }

    #[test]
    fn check_status_needs_exactly_296_bytes() {
        // Liveness: true only for a 296-byte datagram (after forwarding and closing)
        let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
        net.push(33740, &[0u8; PACKET_LEN - 1]);
        assert!(!s.is_running().unwrap());
        net.push(33740, &[0u8; PACKET_LEN]);
        assert!(s.is_running().unwrap());
    }

    #[test]
    fn before_the_first_packet_every_read_raises() {
        // Channel reads fail before the receive step has kept a packet; the dispatcher
        // stores location 0 and lateral_velocity 0, then the top-speed update's speed read ends the tick
        let (mut s, _, _) = source(GranTurismo::Gt7, "192.168.1.50");
        let before = Telemetry {
            rpm: 3000.0,
            pitch: 2.0,
            lateral_velocity: 1.0,
            vehicle: "car".into(),
            location: "x".into(),
            ..Telemetry::off()
        };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { location: String::new(), lateral_velocity: 0.0, ..before });
    }

    #[test]
    fn an_asin_domain_error_makes_every_read_raise() {
        // The attitude's arcsine is outside its domain, which fails before any channel
        let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
        net.push(33740, &encrypt(GranTurismo::Gt7, &plain_packet(1, [1.0, 0.0, 0.0, 1.0], [0.0; 3])));
        assert!(s.is_running().unwrap());
        let before =
            Telemetry { rpm: 3000.0, pitch: 2.0, lateral_velocity: 1.0, location: "x".into(), ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { location: String::new(), lateral_velocity: 0.0, ..before });
    }

    #[test]
    fn non_finite_speed_ends_the_tick_after_lateral_velocity() {
        // A NaN speed has no integer value; car_name is not mapped (0)
        let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
        let mut plain = plain_packet(1, [0.0, 0.0, 0.0, 1.0], [0.0, 0.0, 20.0]);
        plain[0x4C..0x50].copy_from_slice(&f32::NAN.to_le_bytes());
        net.push(33740, &encrypt(GranTurismo::Gt7, &plain));
        assert!(s.is_running().unwrap());
        let before =
            Telemetry { rpm: 3000.0, speed: 9.0, vehicle: "car".into(), lateral_velocity: 1.0, ..Telemetry::off() };
        let mut t = before.clone();
        assert_ends(s.poll(&mut t));
        assert_eq!(t, Telemetry { vehicle: String::new(), lateral_velocity: 0.0, ..before });
    }

    #[test]
    fn pause_and_off_track_read_zero() {
        // Off track or paused: channels read 0
        for flags in [0, 3] {
            let (mut s, net, _) = source(GranTurismo::Gt7, "192.168.1.50");
            net.push(33740, &encrypt(GranTurismo::Gt7, &plain_packet(flags, [0.0, 0.0, 0.0, 1.0], [0.0; 3])));
            s.is_running().unwrap();
            let mut t = Telemetry::off();
            s.poll(&mut t).unwrap();
            assert_eq!(t.rpm, 0.0, "flags {flags}");
        }
    }

    #[test]
    fn accelerations_use_subsecond_interval_and_gains() {
        // Acceleration channels: gains 8 / 10 / -8, over the sub-second part of the interval
        let mut d = Differentiator::new(Duration::ZERO);
        let a = d.acceleration(0, 9.81, Duration::from_millis(500), 8.0);
        assert!((a - 16.0).abs() < 1e-12, "{a}");
        // A 1.5 s gap only counts its 0.5 s sub-second part.
        let b = d.acceleration(0, 19.62, Duration::from_millis(2000), 8.0);
        assert!((b - 16.0).abs() < 1e-12, "{b}");
        // Exactly one second: microseconds == 0 -> 0, but the velocity is still remembered.
        assert_eq!(d.acceleration(0, 50.0, Duration::from_millis(3000), 8.0), 0.0);
        assert_eq!(d.previous_velocity[0], 50.0);
    }

    #[test]
    fn local_velocity_and_attitude_follow_the_quaternion() {
        // Quaternion rotation, conjugate and attitude: yaw of 90 degrees about the vertical axis
        let h = std::f64::consts::FRAC_1_SQRT_2;
        let q = [0.0, h, 0.0, h];
        let a = attitude(q).unwrap();
        assert!((a.yaw - 90.0).abs() < 1e-9, "{a:?}");
        assert!(a.pitch.abs() < 1e-9 && a.roll.abs() < 1e-9);
        let p = GtPacket {
            magic: MAGIC,
            world_velocity: [10.0, 0.0, 0.0],
            rotation: q,
            rpm: 0.0,
            speed: 0.0,
            min_alert_rpm: 0,
            flags: 1,
            suggested_gear_gear: 0,
        };
        let lv = local_velocity(&p);
        assert!((lv[2] - 10.0).abs() < 1e-9 && lv[0].abs() < 1e-9, "{lv:?}");
    }

    #[test]
    fn gimbal_lock_and_invalid_quaternions() {
        // Attitude: the |pitch| close to pi/2 case; arcsine domain error
        let a = attitude([0.5, 0.0, 0.0, 1.0]).unwrap();
        assert_eq!(a.pitch, 90.0);
        assert_eq!((a.roll, a.yaw), (0.0, 0.0));
        assert!(attitude([1.0, 0.0, 0.0, 1.0]).is_none());
    }

    #[test]
    fn forwards_while_listener_running_has_entries() {
        // Forwarding goes to the running listener's PORT_FORWARDING targets
        let sink = ForwardSink::new();
        let net = FakeNetwork::default();
        let settings = GranTurismoSettings::new(GranTurismo::Gt7, "192.168.1.50");
        let mut s = GranTurismoSource::with_parts(
            GranTurismo::Gt7,
            settings,
            Box::new(net.clone()),
            Box::new(FakeClock::default()),
        );
        let (forwarder, parameters) = test_forwarder("gt7", &[sink.target()]);
        parameters.set_listener_running("none");
        s.forwarder = forwarder;
        net.push(33740, &[1; PACKET_LEN]);
        net.push(33740, &[2; PACKET_LEN]);
        s.is_running().unwrap();
        parameters.set_listener_running("gt7");
        s.is_running().unwrap();
        assert_eq!(sink.received(), vec![vec![2; PACKET_LEN]]);
    }

    #[test]
    fn playstation_ip_is_read_on_every_call() {
        // The liveness test, the receive step and channel reads read APP.playstation_ip each time; an empty value disables.
        let net = FakeNetwork::default();
        let mut s = GranTurismoSource::with_parts(
            GranTurismo::Gt7,
            GranTurismoSettings::new(GranTurismo::Gt7, ""),
            Box::new(net.clone()),
            Box::new(FakeClock::default()),
        );
        assert!(!s.is_running().unwrap());
        assert!(net.bound().is_empty(), "'' returns False before any socket");
        s.set_playstation_ip("192.168.1.50");
        net.push(33740, &[1; PACKET_LEN]);
        assert!(s.is_running().unwrap());
        assert_eq!(net.sent()[0].1, "192.168.1.50:33739".parse().unwrap());
    }
}

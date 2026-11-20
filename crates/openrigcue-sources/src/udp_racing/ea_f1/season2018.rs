//! F1 2018 (listener `f12018`): a receiver thread and the dispatcher read the same socket. The thread keeps
//! car-telemetry and participants packets, the dispatcher's receive step keeps motion and car-status packets.
//!
//! The packets are little-endian, byte-packed structures: header 21 bytes,
//! motion 1341, car telemetry 1085, participants 1082,
//! car status 1061, 20 cars.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::{fia_flag_code, Wheel};
use crate::udp_racing::codemasters::kinematics::radians_to_degrees;
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::listener_output::{python_index, python_int, zero_dispatcher_channels};
use crate::udp_racing::parameters::GameErrors;
use crate::udp_racing::socket::{
    socket_creation_failed, DatagramSocket, Endpoint, ImportedPort, ListenerSettings, UdpEndpoint,
};
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// Receive timeout.
const TIMEOUT: Duration = Duration::from_millis(200);
/// Receive buffer size: the motion packet, the largest of the season.
const RECEIVE_SIZE: usize = 1341;
/// Pause of 1/180 s after each datagram the thread handles.
const THREAD_PACE_SECONDS: f64 = 1.0 / 180.0;

/// [`THREAD_PACE_SECONDS`] as a sleep interval (the float is rounded up to whole nanoseconds).
fn thread_pace() -> Duration {
    openrigcue_model::python_time::sleep_duration(THREAD_PACE_SECONDS).unwrap_or_default()
}
/// One-second pause while not connected.
const THREAD_IDLE: Duration = Duration::from_secs(1);
/// Speed factor 0.6214 (binary64 0x3fe3e28240b78034).
const SPEED_FACTOR: f64 = 0.6214;
const CAR_COUNT: usize = 20;

/// Packet layout of the 2018 format.
mod layout {
    pub const HEADER_SIZE: usize = 21;
    pub const PACKET_ID_AT: usize = 3;
    pub const PLAYER_CAR_AT: usize = 20;

    pub const MOTION_ID: u8 = 0;
    pub const PARTICIPANTS_ID: u8 = 4;
    pub const TELEMETRY_ID: u8 = 6;
    pub const STATUS_ID: u8 = 7;

    pub const MOTION_SIZE: usize = 1341;
    pub const PARTICIPANTS_SIZE: usize = 1082;
    pub const TELEMETRY_SIZE: usize = 1085;
    pub const STATUS_SIZE: usize = 1061;

    /// `m_carMotionData[20]` of 60 bytes.
    pub const MOTION_CARS_AT: usize = 21;
    pub const MOTION_STRIDE: usize = 60;
    pub const G_FORCE_LATERAL: usize = 36;
    pub const G_FORCE_LONGITUDINAL: usize = 40;
    pub const G_FORCE_VERTICAL: usize = 44;
    pub const YAW: usize = 48;
    pub const PITCH: usize = 52;
    pub const ROLL: usize = 56;
    pub const SUSPENSION_POSITION_AT: usize = 1221;
    pub const SUSPENSION_VELOCITY_AT: usize = 1237;
    pub const WHEEL_SLIP_AT: usize = 1285;
    pub const LOCAL_VELOCITY_AT: usize = 1301;

    /// The 20 car-telemetry entries of 53 bytes: `m_speed` u16, `m_gear` i8, `m_engineRPM` u16.
    pub const TELEMETRY_CARS_AT: usize = 21;
    pub const TELEMETRY_STRIDE: usize = 53;
    pub const SPEED: usize = 0;
    pub const GEAR: usize = 6;
    pub const ENGINE_RPM: usize = 7;

    /// The 20 car-status entries of 52 bytes: `m_maxRPM` u16, `m_vehicleFiaFlags` i8.
    pub const STATUS_CARS_AT: usize = 21;
    pub const STATUS_STRIDE: usize = 52;
    pub const MAX_RPM: usize = 13;
    pub const FIA_FLAGS: usize = 34;

    /// The 20 participant entries of 53 bytes after `m_numCars`: `m_teamId` u8.
    pub const PARTICIPANTS_CARS_AT: usize = 22;
    pub const PARTICIPANTS_STRIDE: usize = 53;
    pub const TEAM_ID: usize = 2;
}

/// The `car_name` answer where the lookup fails.
const CAR_NAME_RAISED: &str = "-1";

/// The team table (41 slots, indexed by `m_teamId`).
const VEHICLES: [Option<&str>; 41] = [
    Some("Mercedes"),
    Some("Ferrari"),
    Some("Red Bull Racing"),
    Some("Williams"),
    Some("Force India"),
    Some("Renault"),
    Some("Toro Rosso"),
    Some("Haas"),
    Some("McLaren"),
    Some("Sauber"),
    Some("McLaren 1988"),
    Some("McLaren 1991"),
    Some("Williams 1992"),
    Some("Ferrari 1995"),
    Some("Williams 1996"),
    Some("McLaren 1998"),
    Some("Ferrari 2002"),
    Some("Ferrari 2004"),
    Some("Renault 2006"),
    Some("Ferrari 2007"),
    Some("Mclaren 2008"),
    Some("Red Bull 2010"),
    Some("Ferrari 1976"),
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    Some("McLaren 1976"),
    Some("Lotus 1972"),
    Some("Ferrari 1979"),
    Some("McLaren 1982"),
    Some("Williams 2003"),
    Some("Brawn 2009"),
    Some("Lotus 1978"),
];

/// The stored motion, status, telemetry and participants packets (raw packets, `None` until received).
#[derive(Debug, Default, Clone)]
struct Packets {
    motion: Option<Arc<[u8]>>,
    status: Option<Arc<[u8]>>,
    telemetry: Option<Arc<[u8]>>,
    participants: Option<Arc<[u8]>>,
}

/// The player's entry of a per-car array, indexed by the packet's own `m_playerCarIndex`.
fn player_entry(packet: &Option<Arc<[u8]>>, at: usize, stride: usize) -> Option<&[u8]> {
    let packet = packet.as_deref()?;
    let index = python_index(i64::from(packet[layout::PLAYER_CAR_AT]), CAR_COUNT)?;
    let start = at + index * stride;
    packet.get(start..start + stride)
}

fn f32_at(bytes: &[u8], at: usize) -> f64 {
    f64::from(f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]))
}

fn u16_at(bytes: &[u8], at: usize) -> f64 {
    f64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

impl Packets {
    fn motion_car(&self, field: usize) -> Option<f64> {
        player_entry(&self.motion, layout::MOTION_CARS_AT, layout::MOTION_STRIDE).map(|car| f32_at(car, field))
    }

    fn telemetry_car(&self) -> Option<&[u8]> {
        player_entry(&self.telemetry, layout::TELEMETRY_CARS_AT, layout::TELEMETRY_STRIDE)
    }

    fn status_car(&self) -> Option<&[u8]> {
        player_entry(&self.status, layout::STATUS_CARS_AT, layout::STATUS_STRIDE)
    }

    /// One element of a player wheel array of the motion packet.
    fn motion_wheel(&self, array_at: usize, index: usize) -> Option<f64> {
        self.motion.as_deref().map(|motion| f32_at(motion, array_at + 4 * index))
    }

    fn local_velocity(&self, axis: usize) -> Option<f64> {
        self.motion.as_deref().map(|motion| f32_at(motion, layout::LOCAL_VELOCITY_AT + 4 * axis))
    }

    /// The team name of the player's participants entry: `Err(())` where the lookup fails (no
    /// participants packet yet, a player or team index past the end of its list), `Ok(None)` for an unlisted
    /// team id.
    fn car_name(&self) -> Result<Option<&'static str>, ()> {
        let car =
            player_entry(&self.participants, layout::PARTICIPANTS_CARS_AT, layout::PARTICIPANTS_STRIDE).ok_or(())?;
        let index = python_index(i64::from(car[layout::TEAM_ID]), VEHICLES.len()).ok_or(())?;
        Ok(VEHICLES[index])
    }
}

/// A per-car array keeps the packet it was decoded from, while a later lookup can see a new header.
struct NativeCar {
    bytes: Arc<[u8]>,
    at: usize,
    len: usize,
}
impl std::ops::Deref for NativeCar {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.bytes[self.at..self.at + self.len]
    }
}

struct NativeReader<F> {
    packet: F,
}
impl<F: FnMut(u8) -> Option<Arc<[u8]>>> NativeReader<F> {
    /// All getter failures read integer zero, apart from car_name's own text fallback.
    fn native_answer(&mut self, name: &str) -> LegacyValue {
        if name == "car_name" {
            return match self.car_name() {
                Ok(Some(name)) => LegacyValue::Str(name.into()),
                Ok(None) => LegacyValue::None,
                Err(()) => LegacyValue::Str(CAR_NAME_RAISED.into()),
            };
        }
        self.native_value(name).unwrap_or(LegacyValue::Int(0))
    }

    fn native_value(&mut self, name: &str) -> Option<LegacyValue> {
        let float = match name {
            "rpm" => {
                return crate::udp_racing::int_text(u16_at(&self.telemetry_car()?, layout::ENGINE_RPM))
                    .map(LegacyValue::Str)
            }
            "max_rpm" => {
                return crate::udp_racing::int_text(u16_at(&self.status_car()?, layout::MAX_RPM)).map(LegacyValue::Str)
            }
            "gear" => return Some(LegacyValue::Str((self.telemetry_car()?[layout::GEAR] as i8).to_string())),
            "speed" => {
                return crate::udp_racing::int_text(u16_at(&self.telemetry_car()?, layout::SPEED) * SPEED_FACTOR)
                    .map(LegacyValue::Str)
            }
            "flag" => return self.flag(),
            "raw_flag" => {
                (self.packet)(layout::STATUS_ID)?;
                return Some(LegacyValue::Int(i64::from(self.status_car()?[layout::FIA_FLAGS] as i8)));
            }
            "vertical_acceleration" => self.motion_car(layout::G_FORCE_VERTICAL)?,
            "lateral_acceleration" => self.motion_car(layout::G_FORCE_LATERAL)?,
            "longitudinal_acceleration" => self.motion_car(layout::G_FORCE_LONGITUDINAL)? * 10.0,
            "vertical_velocity" => self.local_velocity(1)?,
            "lateral_velocity" => self.local_velocity(0)?,
            "longitudinal_velocity" => self.local_velocity(2)?,
            "yaw" => radians_to_degrees(self.motion_car(layout::YAW)?),
            "roll" => radians_to_degrees(self.motion_car(layout::ROLL)?),
            "pitch" => -radians_to_degrees(self.motion_car(layout::PITCH)?),
            _ => {
                let Some((channel, wheel)) = super::wheel_channel(name) else { return Some(LegacyValue::Int(0)) };
                match channel {
                    "suspension_travel" => {
                        self.motion_wheel(layout::SUSPENSION_POSITION_AT, position_index(wheel))? / 1_000_000.0
                    }
                    "suspension_velocity" => {
                        self.motion_wheel(layout::SUSPENSION_VELOCITY_AT, velocity_and_slip_index(wheel))?
                    }
                    "wheel_slip" => {
                        (self.motion_wheel(layout::WHEEL_SLIP_AT, velocity_and_slip_index(wheel))? * 10.0).abs()
                    }
                    _ => return Some(LegacyValue::Int(0)),
                }
            }
        };
        Some(LegacyValue::Float(float))
    }

    fn car(&mut self, id: u8, at: usize, stride: usize) -> Option<NativeCar> {
        let bytes = (self.packet)(id)?;
        let header = (self.packet)(id)?;
        let index = python_index(i64::from(*header.get(layout::PLAYER_CAR_AT)?), CAR_COUNT)?;
        let at = at + index * stride;
        bytes.get(at..at + stride)?;
        Some(NativeCar { bytes, at, len: stride })
    }
    fn telemetry_car(&mut self) -> Option<NativeCar> {
        self.car(layout::TELEMETRY_ID, layout::TELEMETRY_CARS_AT, layout::TELEMETRY_STRIDE)
    }
    fn status_car(&mut self) -> Option<NativeCar> {
        self.car(layout::STATUS_ID, layout::STATUS_CARS_AT, layout::STATUS_STRIDE)
    }
    fn motion_car(&mut self, field: usize) -> Option<f64> {
        Some(f32_at(&self.car(layout::MOTION_ID, layout::MOTION_CARS_AT, layout::MOTION_STRIDE)?, field))
    }
    fn motion_wheel(&mut self, at: usize, index: usize) -> Option<f64> {
        Some(f32_at(&(self.packet)(layout::MOTION_ID)?, at + index * 4))
    }
    fn local_velocity(&mut self, axis: usize) -> Option<f64> {
        Some(f32_at(&(self.packet)(layout::MOTION_ID)?, layout::LOCAL_VELOCITY_AT + axis * 4))
    }
    fn car_name(&mut self) -> Result<Option<&'static str>, ()> {
        let car =
            self.car(layout::PARTICIPANTS_ID, layout::PARTICIPANTS_CARS_AT, layout::PARTICIPANTS_STRIDE).ok_or(())?;
        let index = python_index(i64::from(car[layout::TEAM_ID]), VEHICLES.len()).ok_or(())?;
        Ok(VEHICLES[index])
    }
    fn flag(&mut self) -> Option<LegacyValue> {
        if (self.packet)(layout::STATUS_ID).is_none() {
            return Some(LegacyValue::Int(0));
        }
        for (flag, output) in [(1, 12), (2, 2), (3, 6), (4, 5)] {
            if self.status_car()?[layout::FIA_FLAGS] as i8 == flag {
                return Some(LegacyValue::Int(output));
            }
        }
        Some(LegacyValue::Int(0))
    }
}

/// Index read for each channel's wheel. Suspension travel follows the game's order (RL, RR, FL, FR); suspension
/// velocity and wheel slip are read with a different table: lr → 2, rr → 0, lf → 3, rf → 1.
fn position_index(wheel: Wheel) -> usize {
    wheel.index()
}

fn velocity_and_slip_index(wheel: Wheel) -> usize {
    match wheel {
        Wheel::RearLeft => 2,
        Wheel::RearRight => 0,
        Wheel::FrontLeft => 3,
        Wheel::FrontRight => 1,
    }
}

/// Writes the channels: every failing lookup and every unmapped channel reads 0.
fn publish(packets: &Packets, t: &mut Telemetry) {
    zero_dispatcher_channels(t);
    let set = |slot: &mut f64, value: Option<f64>| *slot = value.unwrap_or(0.0);
    set(&mut t.rpm, packets.telemetry_car().map(|car| u16_at(car, layout::ENGINE_RPM)));
    set(&mut t.max_rpm, packets.status_car().map(|car| u16_at(car, layout::MAX_RPM)));
    set(&mut t.gear, packets.telemetry_car().map(|car| f64::from(car[layout::GEAR] as i8)));
    set(&mut t.speed, packets.telemetry_car().and_then(|car| python_int(u16_at(car, layout::SPEED) * SPEED_FACTOR)));
    // A failed `car_name` lookup answers the text `"-1"`; an unlisted team is `None`, which
    // `Telemetry::vehicle` holds as empty text.
    t.vehicle = match packets.car_name() {
        Ok(name) => name.unwrap_or_default().to_string(),
        Err(()) => CAR_NAME_RAISED.to_string(),
    };
    // `track_loc` is not mapped: it reads 0.
    t.location.clear();
    for wheel in super::WHEELS {
        let (travel, slip, velocity, _) = wheel.channels(t);
        *travel = packets
            .motion_wheel(layout::SUSPENSION_POSITION_AT, position_index(wheel))
            .map_or(0.0, |p| p / 1_000_000.0);
        *velocity = packets.motion_wheel(layout::SUSPENSION_VELOCITY_AT, velocity_and_slip_index(wheel)).unwrap_or(0.0);
        *slip = packets
            .motion_wheel(layout::WHEEL_SLIP_AT, velocity_and_slip_index(wheel))
            .map_or(0.0, |s| (s * 10.0).abs());
    }
    // The steering channel reads a field this packet layout does not have, so the read always fails.
    t.steering_wheel_angle = 0.0;
    set(&mut t.vertical_acceleration, packets.motion_car(layout::G_FORCE_VERTICAL));
    set(&mut t.lateral_acceleration, packets.motion_car(layout::G_FORCE_LATERAL));
    set(&mut t.longitudinal_acceleration, packets.motion_car(layout::G_FORCE_LONGITUDINAL).map(|g| g * 10.0));
    set(&mut t.vertical_velocity, packets.local_velocity(1));
    set(&mut t.lateral_velocity, packets.local_velocity(0));
    set(&mut t.longitudinal_velocity, packets.local_velocity(2));
    set(&mut t.yaw, packets.motion_car(layout::YAW).map(radians_to_degrees));
    set(&mut t.roll, packets.motion_car(layout::ROLL).map(radians_to_degrees));
    set(&mut t.pitch, packets.motion_car(layout::PITCH).map(|p| -radians_to_degrees(p)));
    let flags = packets.status_car().map(|car| car[layout::FIA_FLAGS] as i8);
    set(&mut t.flag, flags.map(fia_flag_code));
    set(&mut t.raw_flag, flags.map(f64::from));
}

/// State shared by the receiver thread and the dispatcher.
struct Shared {
    socket: Box<dyn DatagramSocket>,
    forwarder: Arc<Forwarder>,
    connected: AtomicBool,
    /// Whether a socket has ever been created.
    socket_assigned: AtomicBool,
    stop: AtomicBool,
    packets: Mutex<Packets>,
    /// Held over each step that must be atomic and that decides which
    /// socket is the current one: clearing the connected flag and closing, the new socket of the liveness test, and
    /// setting the connected flag. A late close of one thread therefore never lands between the other thread's
    /// flag and socket of the same step.
    lifecycle: Mutex<()>,
}

impl Shared {
    fn lifecycle(&self) -> MutexGuard<'_, ()> {
        self.lifecycle.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn packets(&self) -> MutexGuard<'_, Packets> {
        self.packets.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn disconnect(&self) {
        let _lifecycle = self.lifecycle();
        self.connected.store(false, Ordering::Relaxed);
        self.socket.close();
    }

    /// The liveness test: while not connected, rebind and accept the first datagram with a header. A
    /// failed bind or timeout setting adds its text to the game errors, clears the connected flag and closes
    /// the socket; a failed receive or header
    /// step does the same without recording an error. A failed socket creation closes the
    /// old socket; on the first failure there is none yet, and that failure is not handled.
    fn check_status(&self, game_errors: &mut GameErrors) -> Result<bool, SourceError> {
        if self.connected() {
            return Ok(true);
        }
        // The new socket is published in one step. Binding and setting the timeout then
        // run outside the lock: the receiver thread's late "not connected, close"
        // may close the new socket there, and the calls fail into the bind handling below.
        let created = {
            let _lifecycle = self.lifecycle();
            self.socket.create()
        };
        let reopened = created.and_then(|()| self.socket.bind_current());
        if let Err(error) = reopened {
            if socket_creation_failed(&error) {
                let _lifecycle = self.lifecycle();
                self.connected.store(false, Ordering::Relaxed);
                if !self.socket_assigned.load(Ordering::Relaxed) {
                    return Err(
                        DispatcherEnds::new("AttributeError: 'NoneType' object has no attribute 'close'").into()
                    );
                }
                self.socket.close();
                return Ok(false);
            }
            // Binding and setting the timeout run after the new socket has become the current one.
            self.socket_assigned.store(true, Ordering::Relaxed);
            game_errors.record(&error);
            self.disconnect();
            return Ok(false);
        }
        self.socket_assigned.store(true, Ordering::Relaxed);
        Ok(match self.socket.receive(RECEIVE_SIZE) {
            Ok(datagram) if datagram.len() >= layout::HEADER_SIZE => {
                let _lifecycle = self.lifecycle();
                self.connected.store(true, Ordering::Relaxed);
                true
            }
            _ => {
                self.disconnect();
                false
            }
        })
    }

    /// The receive step: read datagrams until a motion packet arrives, keeping car-status packets on
    /// the way. Returns whether a motion packet was stored; storing it empties the game errors.
    fn receive_data(&self, game_errors: &mut GameErrors) -> bool {
        if !self.connected() {
            return false;
        }
        loop {
            let Ok(datagram) = self.socket.receive(RECEIVE_SIZE) else {
                self.disconnect();
                return false;
            };
            let kept = match packet_id(&datagram) {
                Some(layout::MOTION_ID) => match prefix(&datagram, layout::MOTION_SIZE) {
                    Some(motion) => {
                        self.packets().motion = Some(motion);
                        game_errors.clear();
                        return true;
                    }
                    None => false,
                },
                Some(layout::STATUS_ID) => {
                    prefix(&datagram, layout::STATUS_SIZE).map(|status| self.packets().status = Some(status)).is_some()
                }
                Some(_) => true,
                None => false,
            };
            if !kept {
                self.disconnect();
                return false;
            }
        }
    }

    /// One connected iteration of the receiver thread: one datagram, forwarded, telemetry/participants kept.
    fn thread_receive(&self) {
        let Ok(datagram) = self.socket.receive(RECEIVE_SIZE) else {
            self.disconnect();
            return;
        };
        self.forwarder.forward(&datagram);
        let kept = match packet_id(&datagram) {
            Some(layout::TELEMETRY_ID) => {
                prefix(&datagram, layout::TELEMETRY_SIZE).map(|p| self.packets().telemetry = Some(p)).is_some()
            }
            Some(layout::PARTICIPANTS_ID) => {
                prefix(&datagram, layout::PARTICIPANTS_SIZE).map(|p| self.packets().participants = Some(p)).is_some()
            }
            Some(_) => true,
            None => false,
        };
        if !kept {
            self.disconnect();
        }
    }

    /// The receiver thread's loop (until the source is dropped).
    fn run(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            if self.connected() {
                self.thread_receive();
                if self.connected() {
                    std::thread::sleep(thread_pace());
                }
            } else {
                self.idle(THREAD_IDLE);
            }
        }
    }

    /// Sleep while still reacting to a stop request.
    fn idle(&self, duration: Duration) {
        let until = Instant::now() + duration;
        while !self.stop.load(Ordering::Relaxed) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The packet id, when the datagram holds a header.
fn packet_id(datagram: &[u8]) -> Option<u8> {
    (datagram.len() >= layout::HEADER_SIZE).then(|| datagram[layout::PACKET_ID_AT])
}

/// The first `size` bytes of a datagram as one packet: needs at least `size` bytes.
fn prefix(datagram: &[u8], size: usize) -> Option<Arc<[u8]>> {
    datagram.get(..size).map(Arc::from)
}

/// The F1 2018 listener with its receiver thread.
pub struct F1Season2018Source {
    shared: Arc<Shared>,
    receiver: Option<JoinHandle<()>>,
    game_errors: GameErrors,
}

impl F1Season2018Source {
    /// Listen on `0.0.0.0:<settings.port>`; the receiver thread starts right away.
    pub fn new(settings: &ListenerSettings) -> Self {
        let socket = UdpEndpoint::new(Endpoint::any_interface(settings.port, TIMEOUT));
        F1Season2018Source::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Accepts any integer port; the liveness test validates the socket range.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        let socket = UdpEndpoint::from_imported_port(Ipv4Addr::UNSPECIFIED, port, TIMEOUT);
        F1Season2018Source::with_socket(Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Use the given socket and forwarder (tests).
    pub fn with_socket(socket: Box<dyn DatagramSocket>, forwarder: Arc<Forwarder>) -> Self {
        let shared = Arc::new(Shared {
            socket,
            forwarder,
            connected: AtomicBool::new(false),
            socket_assigned: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            packets: Mutex::new(Packets::default()),
            lifecycle: Mutex::new(()),
        });
        let thread_shared = Arc::clone(&shared);
        let receiver = std::thread::Builder::new()
            .name("f12018_main_loop".into())
            .spawn(move || thread_shared.run())
            .map_err(|error| tracing::warn!("f12018: cannot start the receiver: {error}"))
            .ok();
        F1Season2018Source { shared, receiver, game_errors: GameErrors::default() }
    }

    /// The shared game errors set as this listener left it: a failed bind or timeout setting in
    /// the liveness test adds its text, a stored motion packet in the receive step empties the set.
    pub fn game_errors(&self) -> &std::collections::BTreeSet<String> {
        self.game_errors.messages()
    }
}

impl Drop for F1Season2018Source {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.join();
        }
    }
}

impl Source for F1Season2018Source {
    fn id(&self) -> &'static str {
        "f12018"
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        self.shared.check_status(&mut self.game_errors)
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.shared.receive_data(&mut self.game_errors))
    }

    /// A channel read is atomic with respect to the receiver thread, which
    /// cannot store a packet in the middle of it: the call reads one state of the four stored packets.
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let held = self.shared.packets().clone();
        let packet = |id| match id {
            layout::MOTION_ID => held.motion.clone(),
            layout::STATUS_ID => held.status.clone(),
            layout::TELEMETRY_ID => held.telemetry.clone(),
            layout::PARTICIPANTS_ID => held.participants.clone(),
            _ => None,
        };
        Some(NativeReader { packet }.native_answer(name))
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let received = self.shared.receive_data(&mut self.game_errors);
        publish(&self.shared.packets(), telemetry);
        Ok(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct SetupScript(Arc<Mutex<(std::collections::VecDeque<u8>, usize, usize)>>);

    impl DatagramSocket for SetupScript {
        fn reopen(&self) -> std::io::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.1 += 1;
            match state.0.pop_front().expect("unexpected setup attempt") {
                0 => Ok(()),
                1 => Err(crate::udp_racing::socket::socket_creation_error(std::io::Error::other("create failed"))),
                _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "bind(): port must be 0-65535.")),
            }
        }
        fn close(&self) {
            self.0.lock().unwrap().2 += 1;
        }
        fn receive(&self, _: usize) -> std::io::Result<Vec<u8>> {
            Ok(vec![0; 21])
        }
    }

    fn scripted_source(steps: &[u8]) -> (F1Season2018Source, SetupScript) {
        let socket = SetupScript::default();
        socket.0.lock().unwrap().0.extend(steps);
        let shared = Arc::new(Shared {
            socket: Box::new(socket.clone()),
            forwarder: Arc::new(Forwarder::default()),
            connected: AtomicBool::new(false),
            socket_assigned: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            packets: Mutex::new(Packets::default()),
            lifecycle: Mutex::new(()),
        });
        (F1Season2018Source { shared, receiver: None, game_errors: GameErrors::default() }, socket)
    }

    #[test]
    fn native_2018_first_creation_error_retains_attribute_failure_and_previous_data() {
        let (mut source, socket) = scripted_source(&[1]);
        source.shared.packets().motion = Some(packet(layout::MOTION_ID, layout::MOTION_SIZE, 0).into());
        source.game_errors.record(&std::io::Error::other("previous error"));
        let error = source.is_running().unwrap_err();
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error));
        assert!(error.to_string().contains("AttributeError: 'NoneType' object has no attribute 'close'"));
        assert_eq!(source.game_errors().iter().cloned().collect::<Vec<_>>(), ["previous error"]);
        assert!(source.shared.packets().motion.is_some());
        assert_eq!(socket.0.lock().unwrap().2, 0);
    }

    #[test]
    fn native_2018_first_creation_failure_ends_dispatcher_before_later_listener() {
        use crate::derive::{
            AttitudeSource, DamsMode, DamsSettings, DispatchSettings, Dispatcher, GameDirectory, IntellituneReference,
            ProfileSettings, RefreshRates, RuntimeInputs, ScanHit, SourceListener, SpeedUnit, TickInput,
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
        struct Later(Arc<std::sync::atomic::AtomicUsize>);
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
        let (source, socket) = scripted_source(&[1]);
        let state = source.shared.clone();
        let later = Arc::new(std::sync::atomic::AtomicUsize::new(0));
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
            ScanHit { game_id: "f12018".into(), listener_id: "f12018".into() },
            ScanHit { game_id: "later".into(), listener_id: "later".into() },
        ];
        let input = TickInput { hits: &hits, settings: &settings, profile: &profile, runtime: &runtime, games: &Games };
        let outcome = dispatcher.tick(&input);
        assert!(dispatcher.has_ended(), "None.close() escapes the initial socket creation catch");
        assert_eq!(later.load(Ordering::Relaxed), 0);
        assert_eq!(socket.0.lock().unwrap().1, 1);
        assert_eq!(socket.0.lock().unwrap().2, 0);
        assert!(!state.connected());
        assert!(state.packets().motion.is_none());
        assert_eq!(outcome.events, [crate::derive::DispatchEvent::GameRunning("f12018".into())]);
        assert_eq!(outcome.sleep, Duration::ZERO);
        assert!(dispatcher.telemetry_writes().is_empty());
        dispatcher.tick(&input);
        assert_eq!(socket.0.lock().unwrap().1, 1, "dispatcher remains ended");
    }

    #[test]
    fn native_2018_later_creation_failure_closes_existing_socket_without_recording_it() {
        let (mut source, socket) = scripted_source(&[0, 1]);
        assert!(source.is_running().unwrap());
        source.shared.disconnect();
        assert!(!source.is_running().unwrap());
        assert!(source.game_errors().is_empty(), "creation catch does not add game_errors");
        assert_eq!(socket.0.lock().unwrap().1, 2);
        assert_eq!(socket.0.lock().unwrap().2, 2);
    }

    #[test]
    fn native_2018_bind_failure_publishes_socket_before_later_creation_catch() {
        let (mut source, socket) = scripted_source(&[2, 1, 0]);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().iter().cloned().collect::<Vec<_>>(), ["bind(): port must be 0-65535."]);
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().len(), 1, "later creation error keeps prior bind message");
        assert!(source.is_running().unwrap(), "checkStatus retries after caught failures");
        assert_eq!(socket.0.lock().unwrap().1, 3);
        assert_eq!(socket.0.lock().unwrap().2, 2);
    }

    #[test]
    fn native_rpm_returns_text() {
        let socket = ScriptedSocket::new();
        let shared = Arc::new(shared(&socket));
        let mut telemetry = packet(layout::TELEMETRY_ID, layout::TELEMETRY_SIZE, 0);
        telemetry[layout::TELEMETRY_CARS_AT + layout::ENGINE_RPM..][..2].copy_from_slice(&9000u16.to_le_bytes());
        shared.packets().telemetry = Some(telemetry.into());
        let source = F1Season2018Source { shared, receiver: None, game_errors: GameErrors::default() };
        assert_eq!(source.native_channel("rpm"), Some(openrigcue_model::native_value::LegacyValue::Str("9000".into())));
    }

    #[test]
    fn native_2018_keeps_array_packet_and_rereads_header_before_indexing() {
        let mut old = packet(layout::TELEMETRY_ID, layout::TELEMETRY_SIZE, 0);
        let at = layout::TELEMETRY_CARS_AT + 3 * layout::TELEMETRY_STRIDE + layout::ENGINE_RPM;
        old[at..at + 2].copy_from_slice(&7333_u16.to_le_bytes());
        let old: Arc<[u8]> = old.into();
        let newer: Arc<[u8]> = packet(layout::TELEMETRY_ID, layout::TELEMETRY_SIZE, 3).into();
        let mut reads = std::collections::VecDeque::from([old, newer]);
        let mut native = NativeReader {
            packet: |id| {
                assert_eq!(id, layout::TELEMETRY_ID);
                reads.pop_front()
            },
        };
        assert_eq!(native.native_answer("rpm"), LegacyValue::Str("7333".into()));
        assert!(reads.is_empty());
    }

    #[test]
    fn native_2018_flag_tests_take_current_status_and_names_keep_their_catch() {
        let status = |flag| {
            let mut p = packet(layout::STATUS_ID, layout::STATUS_SIZE, 0);
            p[layout::STATUS_CARS_AT + layout::FIA_FLAGS] = flag;
            Arc::<[u8]>::from(p)
        };
        let mut reads = std::collections::VecDeque::from([status(1), status(0), status(0), status(2), status(2)]);
        let mut native = NativeReader {
            packet: |id| {
                assert_eq!(id, layout::STATUS_ID);
                reads.pop_front()
            },
        };
        assert_eq!(native.native_answer("flag"), LegacyValue::Int(2));
        assert!(reads.is_empty());
        let mut native = NativeReader { packet: |_| None };
        assert_eq!(native.native_answer("car_name"), LegacyValue::Str("-1".into()));
        assert_eq!(native.native_answer("rpm"), LegacyValue::Int(0));
        assert_eq!(native.native_answer("raw_flag"), LegacyValue::Int(0));
    }

    #[test]
    fn native_2018_names_and_live_packet_types() {
        use crate::derive::{Listener, SourceListener};
        let socket = ScriptedSocket::new();
        let shared = Arc::new(shared(&socket));
        let source =
            F1Season2018Source { shared: Arc::clone(&shared), receiver: None, game_errors: GameErrors::default() };
        let listener = SourceListener::new(source);
        assert_eq!(listener.native_channel("car_name"), Some(LegacyValue::Str("-1".into())));
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Int(0)));
        let mut telemetry = packet(layout::TELEMETRY_ID, layout::TELEMETRY_SIZE, 0);
        let at = layout::TELEMETRY_CARS_AT;
        telemetry[at + layout::ENGINE_RPM..][..2].copy_from_slice(&9000u16.to_le_bytes());
        telemetry[at + layout::SPEED..][..2].copy_from_slice(&300u16.to_le_bytes());
        telemetry[at + layout::GEAR] = (-1i8) as u8;
        let mut status = packet(layout::STATUS_ID, layout::STATUS_SIZE, 0);
        status[layout::STATUS_CARS_AT + layout::MAX_RPM..][..2].copy_from_slice(&12000u16.to_le_bytes());
        status[layout::STATUS_CARS_AT + layout::FIA_FLAGS] = 1;
        let mut participants = packet(layout::PARTICIPANTS_ID, layout::PARTICIPANTS_SIZE, 0);
        participants[layout::PARTICIPANTS_CARS_AT + layout::TEAM_ID] = 25;
        {
            let mut packets = shared.packets();
            packets.telemetry = Some(telemetry.into());
            packets.status = Some(status.into());
            packets.participants = Some(participants.clone().into());
            packets.motion = Some(packet(layout::MOTION_ID, layout::MOTION_SIZE, 0).into());
        }
        for (name, expected) in [
            ("rpm", LegacyValue::Str("9000".into())),
            ("max_rpm", LegacyValue::Str("12000".into())),
            ("speed", LegacyValue::Str("186".into())),
            ("gear", LegacyValue::Str("-1".into())),
            ("flag", LegacyValue::Int(12)),
            ("raw_flag", LegacyValue::Int(1)),
            ("car_name", LegacyValue::None),
            ("track_loc", LegacyValue::Int(0)),
            ("steering_wheel_angle", LegacyValue::Int(0)),
            ("vertical_acceleration", LegacyValue::Float(0.0)),
        ] {
            assert_eq!(listener.native_channel(name), Some(expected), "{name}");
        }
        participants[layout::PARTICIPANTS_CARS_AT + layout::TEAM_ID] = 39;
        shared.packets().participants = Some(participants.into());
        assert_eq!(listener.native_channel("car_name"), Some(LegacyValue::Str("Brawn 2009".into())));
    }
    use crate::udp_racing::socket::scripted::ScriptedSocket;

    fn shared(socket: &ScriptedSocket) -> Shared {
        Shared {
            socket: Box::new(socket.clone()),
            forwarder: Arc::new(Forwarder::default()),
            connected: AtomicBool::new(false),
            socket_assigned: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            packets: Mutex::new(Packets::default()),
            lifecycle: Mutex::new(()),
        }
    }

    fn packet(id: u8, size: usize, player: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; size];
        bytes[..2].copy_from_slice(&2018u16.to_le_bytes());
        bytes[layout::PACKET_ID_AT] = id;
        bytes[layout::PLAYER_CAR_AT] = player;
        bytes
    }

    fn put_f32(bytes: &mut [u8], at: usize, value: f32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn check_status_needs_a_datagram_with_a_header() {
        // Liveness: one receive; the header needs 21 bytes.
        let socket = ScriptedSocket::new();
        let s = shared(&socket);
        socket.push(&[0u8; 20]);
        assert!(!s.check_status(&mut GameErrors::default()).unwrap());
        socket.push(&[0u8; 21]);
        assert!(s.check_status(&mut GameErrors::default()).unwrap());
        assert!(s.check_status(&mut GameErrors::default()).unwrap());
        assert_eq!(socket.activity().reopened, 2);
        assert_eq!(socket.activity().receives, vec![1341, 1341]);
    }

    #[test]
    fn bind_failures_go_to_game_errors_until_a_motion_packet() {
        // A failed bind or timeout setting adds its text to the game errors;
        // a failed receive or header adds nothing; the receive step empties the set after a
        // stored motion packet.
        let socket = ScriptedSocket::new();
        let s = shared(&socket);
        let mut errors = GameErrors::default();
        socket.refuse_bind(true);
        assert!(!s.check_status(&mut errors).unwrap());
        assert_eq!(errors.messages().iter().collect::<Vec<_>>(), [crate::udp_racing::parameters::PORT_IN_USE]);
        socket.refuse_bind(false);
        assert!(!s.check_status(&mut errors).unwrap(), "a receive timeout");
        assert_eq!(errors.messages().len(), 1);
        socket.push(&[0u8; 21]);
        assert!(s.check_status(&mut errors).unwrap());
        socket.push(&packet(layout::STATUS_ID, layout::STATUS_SIZE, 0));
        socket.push(&packet(layout::MOTION_ID, layout::MOTION_SIZE, 0));
        assert!(s.receive_data(&mut errors));
        assert!(errors.messages().is_empty());
    }

    #[test]
    fn receive_data_reads_until_a_motion_packet() {
        // Receive step: status packets are kept, other ids skipped, motion ends the call.
        let socket = ScriptedSocket::new();
        let s = shared(&socket);
        socket.push(&[0u8; 21]);
        assert!(s.check_status(&mut GameErrors::default()).unwrap());
        let mut status = packet(layout::STATUS_ID, layout::STATUS_SIZE, 2);
        status[layout::STATUS_CARS_AT + 2 * layout::STATUS_STRIDE + layout::MAX_RPM..][..2]
            .copy_from_slice(&11_500u16.to_le_bytes());
        status[layout::STATUS_CARS_AT + 2 * layout::STATUS_STRIDE + layout::FIA_FLAGS] = 1;
        socket.push(&status);
        socket.push(&packet(layout::TELEMETRY_ID, layout::TELEMETRY_SIZE, 0));
        let mut motion = packet(layout::MOTION_ID, layout::MOTION_SIZE, 0);
        put_f32(&mut motion, layout::MOTION_CARS_AT + layout::PITCH, 0.1);
        put_f32(&mut motion, layout::MOTION_CARS_AT + layout::G_FORCE_LONGITUDINAL, -0.5);
        put_f32(&mut motion, layout::SUSPENSION_POSITION_AT + 4, 2_000_000.0); // RR
        put_f32(&mut motion, layout::SUSPENSION_VELOCITY_AT + 8, 1.5); // index 2 -> read by suspension_velocity_lr
        put_f32(&mut motion, layout::WHEEL_SLIP_AT + 12, -0.2); // index 3 -> read by wheel_slip_lf
        put_f32(&mut motion, layout::LOCAL_VELOCITY_AT, 4.0);
        socket.push(&motion);
        socket.push(b"left for the next call");
        assert!(s.receive_data(&mut GameErrors::default()));
        assert_eq!(socket.pending(), 1);
        let mut t = Telemetry::off();
        publish(&s.packets(), &mut t);
        assert_eq!((t.max_rpm, t.flag, t.raw_flag), (11_500.0, 12.0, 1.0));
        assert_eq!(t.pitch, -radians_to_degrees(f64::from(0.1f32)));
        assert_eq!(t.longitudinal_acceleration, -5.0);
        assert_eq!(t.suspension_travel_rr, 2.0);
        assert_eq!((t.suspension_velocity_lr, t.suspension_velocity_lf), (1.5, 0.0));
        assert_eq!(t.wheel_slip_lf, (f64::from(-0.2f32) * 10.0).abs());
        assert_eq!(t.lateral_velocity, 4.0);
        assert_eq!((t.rpm, t.gear, t.steering_wheel_angle), (0.0, 0.0, 0.0));
    }

    #[test]
    fn receive_failure_or_short_packet_disconnects() {
        let socket = ScriptedSocket::new();
        let s = shared(&socket);
        socket.push(&[0u8; 21]);
        s.check_status(&mut GameErrors::default()).unwrap();
        socket.push(&packet(layout::MOTION_ID, 100, 0));
        assert!(!s.receive_data(&mut GameErrors::default()));
        assert!(!s.connected());
        assert!(!socket.is_open());
    }

    #[test]
    fn thread_keeps_telemetry_and_participants() {
        // Receiver thread: ids 6 and 4 are stored from the thread.
        let socket = ScriptedSocket::new();
        let s = shared(&socket);
        socket.push(&[0u8; 21]);
        s.check_status(&mut GameErrors::default()).unwrap();
        let mut telemetry = packet(layout::TELEMETRY_ID, layout::TELEMETRY_SIZE, 1);
        let car = layout::TELEMETRY_CARS_AT + layout::TELEMETRY_STRIDE;
        telemetry[car..car + 2].copy_from_slice(&300u16.to_le_bytes());
        telemetry[car + layout::GEAR] = 7;
        telemetry[car + layout::ENGINE_RPM..car + layout::ENGINE_RPM + 2].copy_from_slice(&10_400u16.to_le_bytes());
        let mut participants = packet(layout::PARTICIPANTS_ID, layout::PARTICIPANTS_SIZE, 0);
        participants[layout::PARTICIPANTS_CARS_AT + layout::TEAM_ID] = 39;
        socket.push(&telemetry);
        socket.push(&participants);
        s.thread_receive();
        s.thread_receive();
        let mut t = Telemetry::off();
        publish(&s.packets(), &mut t);
        assert_eq!((t.rpm, t.gear, t.speed), (10_400.0, 7.0, 186.0)); // 300 * 0.6214, truncated
        assert_eq!(t.vehicle, "Brawn 2009");
        // A short telemetry packet in the thread disconnects.
        socket.push(&packet(layout::TELEMETRY_ID, 500, 0));
        s.thread_receive();
        assert!(!s.connected());
    }

    #[test]
    fn unlisted_team_ids_have_no_name() {
        let mut packets = Packets::default();
        let mut participants = packet(layout::PARTICIPANTS_ID, layout::PARTICIPANTS_SIZE, 0);
        participants[layout::PARTICIPANTS_CARS_AT + layout::TEAM_ID] = 25;
        packets.participants = Some(participants.clone().into());
        assert_eq!(packets.car_name(), Ok(None));
        participants[layout::PARTICIPANTS_CARS_AT + layout::TEAM_ID] = 41;
        packets.participants = Some(participants.into());
        assert_eq!(packets.car_name(), Err(()), "vehicles[41] raises IndexError");
    }

    #[test]
    fn a_raising_car_name_lookup_answers_minus_one() {
        // A failed car_name lookup answers "-1", e.g. before the
        // first participants packet.
        let mut t = Telemetry::off();
        publish(&Packets::default(), &mut t);
        assert_eq!(t.vehicle, "-1");
        let packets = Packets {
            participants: Some(packet(layout::PARTICIPANTS_ID, layout::PARTICIPANTS_SIZE, 25).into()),
            ..Packets::default()
        };
        publish(&packets, &mut t);
        assert_eq!(t.vehicle, "-1", "player index 25 is past participants_data[20]");
    }

    /// A loopback endpoint whose first `bind_current` is preceded by the receiver thread's late
    /// "not connected, close", which lands on the socket the liveness test has just published.
    struct LateClose {
        endpoint: UdpEndpoint,
        shared: Arc<std::sync::OnceLock<std::sync::Weak<Shared>>>,
        closes_before_bind: std::sync::atomic::AtomicUsize,
    }

    impl DatagramSocket for LateClose {
        fn reopen(&self) -> std::io::Result<()> {
            panic!("checkStatus creates and binds in separate steps")
        }
        fn create(&self) -> std::io::Result<()> {
            self.endpoint.create()
        }
        fn bind_current(&self) -> std::io::Result<()> {
            if self.closes_before_bind.load(Ordering::Relaxed) > 0 {
                self.closes_before_bind.fetch_sub(1, Ordering::Relaxed);
                let shared = self.shared.get().and_then(std::sync::Weak::upgrade).unwrap();
                assert!(shared.lifecycle.try_lock().is_ok(), "bind runs without the lifecycle lock");
                shared.disconnect();
            }
            self.endpoint.bind_current()
        }
        fn close(&self) {
            self.endpoint.close();
        }
        fn receive(&self, max_len: usize) -> std::io::Result<Vec<u8>> {
            self.endpoint.receive(max_len)
        }
    }

    #[test]
    fn native_2018_late_close_between_socket_and_bind_fails_into_the_bind_handler() {
        let cell = Arc::new(std::sync::OnceLock::new());
        let socket = LateClose {
            endpoint: UdpEndpoint::new(Endpoint::loopback(0, Duration::from_millis(30))),
            shared: cell.clone(),
            closes_before_bind: std::sync::atomic::AtomicUsize::new(1),
        };
        let shared = Arc::new(Shared {
            socket: Box::new(socket),
            forwarder: Arc::new(Forwarder::default()),
            connected: AtomicBool::new(false),
            socket_assigned: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            packets: Mutex::new(Packets::default()),
            lifecycle: Mutex::new(()),
        });
        cell.set(Arc::downgrade(&shared)).unwrap();
        let mut source = F1Season2018Source { shared, receiver: None, game_errors: GameErrors::default() };
        // Binding the closed socket fails: the text is added to the game errors and
        // the dispatcher goes on (there is a socket object: the new, closed one).
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().iter().cloned().collect::<Vec<_>>(), ["[Errno 9] Bad file descriptor"]);
        assert!(!source.shared.connected());
        assert!(source.shared.socket_assigned.load(Ordering::Relaxed));
        // The next pass creates and binds again; the receive times out without a header.
        assert!(!source.is_running().unwrap());
        assert_eq!(source.game_errors().len(), 1);
    }

    #[test]
    fn source_with_thread_starts_and_stops() {
        let socket = ScriptedSocket::new();
        let mut source = F1Season2018Source::with_socket(Box::new(socket.clone()), Arc::new(Forwarder::default()));
        assert!(!source.is_running().unwrap());
        assert_eq!(source.id(), "f12018");
        drop(source);
    }
}

#[cfg(test)]
mod pace_tests {
    /// A pause of 1/180 s (binary64 0x3f76c16c16c16c17) after each datagram the thread handles.
    #[test]
    fn thread_pace_is_the_original_sleep() {
        assert_eq!(super::THREAD_PACE_SECONDS.to_bits(), 0x3f76_c16c_16c1_6c17);
        assert_eq!(Ok(super::thread_pace()), openrigcue_model::python_time::sleep_duration(1.0 / 180.0));
        #[cfg(not(windows))]
        assert_eq!(super::thread_pace().as_nanos(), 5_555_556);
    }
}

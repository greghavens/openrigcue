//! F1 2019 to F1 25 (listeners `f12019` … `f12025`): a receiver thread keeps the newest packet of every type;
//! the dispatcher reads channels from them.

use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::layout::{car_motion, car_status, car_telemetry, packet_id, CarArray, SeasonSpec};
use super::names::lookup;
use super::{fia_flag_code, rumble_strength, Wheel, WHEELS};
use crate::udp_racing::codemasters::kinematics::radians_to_degrees;
use crate::udp_racing::forward::Forwarder;
use crate::udp_racing::listener_output::{python_index, python_int, zero_dispatcher_channels};
use crate::udp_racing::socket::{DatagramSocket, Endpoint, ImportedPort, ListenerSettings, UdpEndpoint};
use crate::{Source, SourceError};

/// Receive timeout.
const TIMEOUT: Duration = Duration::from_secs(1);
/// Receive buffer size.
const RECEIVE_SIZE: usize = 2048;

/// One of the F1 2019–2025 games.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Season {
    Y2019,
    Y2020,
    Y2021,
    Y2022,
    Y2023,
    Y2024,
    Y2025,
}

impl Season {
    fn spec(self) -> &'static SeasonSpec {
        use super::layout::*;
        match self {
            Season::Y2019 => &F1_2019,
            Season::Y2020 => &F1_2020,
            Season::Y2021 => &F1_2021,
            Season::Y2022 => &F1_2022,
            Season::Y2023 => &F1_2023,
            Season::Y2024 => &F1_2024,
            Season::Y2025 => &F1_2025,
        }
    }

    /// Listener id and `NETWORK_PORT` key (`"f12019"` …).
    pub fn id(self) -> &'static str {
        self.spec().id
    }

    /// The default port configured for this game.
    pub fn default_port(self) -> u16 {
        self.spec().default_port
    }
}

/// The packet store (packet id -> newest packet, in first-insertion order) and the player car index.
#[derive(Debug)]
struct PacketStore {
    packets: Vec<(u8, Arc<[u8]>)>,
    /// The player car index: -1 until a channel read first reads a header (from 2022 it feeds the receiver's team check).
    player_car: i64,
}

impl PacketStore {
    fn new() -> Self {
        PacketStore { packets: Vec::new(), player_car: -1 }
    }

    /// Stores a packet by id: a known id keeps its place in iteration order.
    fn insert(&mut self, id: u8, packet: Vec<u8>) {
        let packet: Arc<[u8]> = packet.into();
        match self.packets.iter_mut().find(|(known, _)| *known == id) {
            Some(slot) => slot.1 = packet,
            None => self.packets.push((id, packet)),
        }
    }

    fn get(&self, id: u8) -> Option<&[u8]> {
        self.packets.iter().find(|(known, _)| *known == id).map(|(_, packet)| packet.as_ref())
    }

    /// The first stored packet in iteration order.
    fn first(&self) -> Option<&[u8]> {
        self.packets.first().map(|(_, packet)| packet.as_ref())
    }
}

/// State shared by the receiver thread and the dispatcher.
struct Shared {
    spec: &'static SeasonSpec,
    forwarder: Arc<Forwarder>,
    connected: AtomicBool,
    stop: AtomicBool,
    store: Mutex<PacketStore>,
}

impl Shared {
    fn new(spec: &'static SeasonSpec, forwarder: Arc<Forwarder>) -> Self {
        Shared {
            spec,
            forwarder,
            connected: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            store: Mutex::new(PacketStore::new()),
        }
    }

    fn store(&self) -> MutexGuard<'_, PacketStore> {
        self.store.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One iteration of the receiver loop after a receive returned or failed.
    fn on_receive(&self, received: io::Result<Vec<u8>>) {
        let datagram = match received {
            Ok(datagram) => datagram,
            Err(_) => {
                // The store reset and the cleared connected flag are published together: no getter runs
                // between them.
                let mut store = self.store();
                if self.spec.forget_packets_on_receive_failure {
                    store.packets.clear();
                }
                self.connected.store(false, Ordering::Relaxed);
                return;
            }
        };
        self.forwarder.forward(&datagram);
        {
            // The team test and the connected flag it decides are one atomic step.
            let store = self.store();
            let connected = !self.spec.require_known_team || player_team_is_known(self.spec, &store);
            self.connected.store(connected, Ordering::Relaxed);
        }
        // Decoding runs outside the lock: other threads may run while it
        // works, before the packet is stored.
        if let Some(id) = self.spec.accept(&datagram) {
            self.store().insert(id, datagram);
        }
    }

    /// The receiver loop: bind once, then receive until the source is dropped.
    fn run(&self, socket: &dyn DatagramSocket) {
        if let Err(error) = socket.reopen() {
            tracing::warn!("{}: cannot bind the telemetry port: {error}", self.spec.id);
            return;
        }
        while !self.stop.load(Ordering::Relaxed) {
            self.on_receive(socket.receive(RECEIVE_SIZE));
        }
    }
}

/// The receiver's test that the player's team id is in the team table (F1 22 on).
fn player_team_is_known(spec: &SeasonSpec, store: &PacketStore) -> bool {
    let reader = Reader { spec, store, player: store.player_car };
    reader
        .car(packet_id::PARTICIPANTS, spec.participants)
        .and_then(|car| lookup(spec.teams, car[spec.team_id_at]))
        .is_some()
}

/// Field access into the stored packets for one player-car index.
struct Reader<'a> {
    spec: &'a SeasonSpec,
    store: &'a PacketStore,
    player: i64,
}

impl Reader<'_> {
    /// The player's entry of a per-car array (negative indices count from the end).
    fn car(&self, id: u8, array: CarArray) -> Option<&[u8]> {
        let packet = self.store.get(id)?;
        let index = python_index(self.player, self.spec.car_count)?;
        let at = array.at + index * array.stride;
        packet.get(at..at + array.stride)
    }

    fn motion(&self, field: usize) -> Option<f64> {
        let car = self.car(packet_id::MOTION, CarArray { at: self.spec.motion_cars_at, stride: car_motion::STRIDE })?;
        Some(f32_at(car, field))
    }

    fn telemetry(&self) -> Option<&[u8]> {
        self.car(packet_id::CAR_TELEMETRY, self.spec.telemetry)
    }

    fn status(&self) -> Option<&[u8]> {
        self.car(packet_id::CAR_STATUS, self.spec.status)
    }

    /// A player wheel array (RL, RR, FL, FR) of the wheel-data packet.
    fn wheel(&self, array_at: usize, wheel: Wheel) -> Option<f64> {
        let packet = self.store.get(self.spec.wheel_data.packet_id)?;
        Some(f32_at(packet, array_at + 4 * wheel.index()))
    }

    /// `localVelocity{X,Y,Z}` (axis 0, 1, 2).
    fn local_velocity(&self, axis: usize) -> Option<f64> {
        let packet = self.store.get(self.spec.wheel_data.packet_id)?;
        Some(f32_at(packet, self.spec.wheel_data.local_velocity_at + 4 * axis))
    }
}

/// A packet lookup retains that immutable decoded packet while later lookups see receiver updates.
struct NativePacket {
    bytes: Arc<[u8]>,
    at: usize,
    len: usize,
}

impl std::ops::Deref for NativePacket {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.bytes[self.at..self.at + self.len]
    }
}

struct NativeReader<F, P> {
    spec: &'static SeasonSpec,
    initial_player: i64,
    packet: F,
    player: P,
}

impl<F: FnMut(u8) -> Option<Arc<[u8]>>, P: FnMut() -> i64> NativeReader<F, P> {
    fn native_value(&mut self, name: &str) -> Option<LegacyValue> {
        let float = match name {
            "rpm" => return Some(LegacyValue::Int(u16_at(&self.telemetry()?, car_telemetry::ENGINE_RPM) as i64)),
            "max_rpm" => return Some(LegacyValue::Int(u16_at(&self.status()?, car_status::MAX_RPM) as i64)),
            "gear" => return Some(LegacyValue::Int(i64::from(i8_at(&self.telemetry()?, car_telemetry::GEAR)))),
            "speed" => {
                return crate::udp_racing::native_int(
                    u16_at(&self.telemetry()?, car_telemetry::SPEED) * self.spec.speed_factor,
                )
            }
            "car_name" => {
                let car = self.car(packet_id::PARTICIPANTS, self.spec.participants)?;
                return lookup(self.spec.teams, car[self.spec.team_id_at]).map(|name| LegacyValue::Str(name.into()));
            }
            "track_loc" => {
                let packet = (self.packet)(packet_id::SESSION)?;
                return lookup(self.spec.tracks, i8_at(&packet, self.spec.session_track_id_at))
                    .map(|name| LegacyValue::Str(name.into()));
            }
            "steering_wheel_angle" => f32_at(&self.telemetry()?, car_telemetry::STEER) * 10.0,
            "vertical_acceleration" => self.motion(car_motion::G_FORCE_VERTICAL)?,
            "lateral_acceleration" => self.motion(car_motion::G_FORCE_LATERAL)?,
            "longitudinal_acceleration" => self.motion(car_motion::G_FORCE_LONGITUDINAL)? * 10.0,
            "yaw" => radians_to_degrees(self.motion(car_motion::YAW)?),
            "roll" => radians_to_degrees(self.motion(car_motion::ROLL)?),
            "pitch" => {
                radians_to_degrees(self.motion(car_motion::PITCH)?) * if self.spec.invert_pitch { -1.0 } else { 1.0 }
            }
            "vertical_velocity" => self.local_velocity(1)?,
            "lateral_velocity" => -self.local_velocity(0)?,
            "longitudinal_velocity" => {
                self.local_velocity(2)? * if self.spec.invert_longitudinal_velocity { -1.0 } else { 1.0 }
            }
            "flag" => {
                return Some(LegacyValue::Int(fia_flag_code(i8_at(&self.status()?, self.spec.fia_flags_at)) as i64))
            }
            "raw_flag" => return Some(LegacyValue::Int(i64::from(i8_at(&self.status()?, self.spec.fia_flags_at)))),
            "fuel_level" => {
                let fuel = f32_at(&self.status()?, car_status::FUEL_IN_TANK);
                let capacity = f32_at(&self.status()?, car_status::FUEL_CAPACITY);
                if capacity == 0.0 {
                    return None;
                }
                fuel / capacity * 100.0
            }
            _ => {
                let Some((channel, wheel)) = super::wheel_channel(name) else { return Some(LegacyValue::Int(0)) };
                let wheels = self.spec.wheel_data;
                match channel {
                    "suspension_travel" => self.wheel(wheels.suspension_position_at, wheel)? / 10000.0,
                    "suspension_velocity" => self.wheel(wheels.suspension_velocity_at, wheel)?,
                    "wheel_slip" => (self.wheel(wheels.wheel_slip_at, wheel)? * 10.0).abs(),
                    "rumble_pitch" => {
                        return Some(LegacyValue::Int(rumble_strength(
                            self.telemetry()?[self.spec.surface_type_at + wheel.index()],
                        ) as i64))
                    }
                    _ => unreachable!(),
                }
            }
        };
        Some(LegacyValue::Float(float))
    }

    fn car(&mut self, id: u8, array: CarArray) -> Option<NativePacket> {
        let bytes = (self.packet)(id)?;
        // 2019–2021 use the player index this read started with; from 2022 the shared player car index is read
        // again for every lookup.
        let player = if self.spec.packet_format >= 2022 { (self.player)() } else { self.initial_player };
        let index = python_index(player, self.spec.car_count)?;
        let at = array.at + index * array.stride;
        bytes.get(at..at + array.stride)?;
        Some(NativePacket { bytes, at, len: array.stride })
    }

    fn motion(&mut self, field: usize) -> Option<f64> {
        let car = self.car(packet_id::MOTION, CarArray { at: self.spec.motion_cars_at, stride: car_motion::STRIDE })?;
        Some(f32_at(&car, field))
    }

    fn telemetry(&mut self) -> Option<NativePacket> {
        self.car(packet_id::CAR_TELEMETRY, self.spec.telemetry)
    }

    fn status(&mut self) -> Option<NativePacket> {
        self.car(packet_id::CAR_STATUS, self.spec.status)
    }

    fn wheel(&mut self, at: usize, wheel: Wheel) -> Option<f64> {
        let packet = (self.packet)(self.spec.wheel_data.packet_id)?;
        Some(f32_at(&packet, at + 4 * wheel.index()))
    }

    fn local_velocity(&mut self, axis: usize) -> Option<f64> {
        let packet = (self.packet)(self.spec.wheel_data.packet_id)?;
        Some(f32_at(&packet, self.spec.wheel_data.local_velocity_at + 4 * axis))
    }
}

fn f32_at(bytes: &[u8], at: usize) -> f64 {
    f64::from(f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]))
}

fn u16_at(bytes: &[u8], at: usize) -> f64 {
    f64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn i8_at(bytes: &[u8], at: usize) -> i8 {
    i8::from_le_bytes([bytes[at]])
}

/// Writes every registry channel: each
/// channel whose packet is missing or whose lookup fails reads 0, as does every channel the listener does
/// not map.
fn publish(spec: &SeasonSpec, store: &mut PacketStore, t: &mut Telemetry) {
    zero_dispatcher_channels(t);
    let Some(first) = store.first() else {
        t.vehicle.clear();
        t.location.clear();
        return;
    };
    let player = i64::from(first[spec.header.player_car_at]);
    store.player_car = player;
    let r = Reader { spec, store, player };
    let set = |slot: &mut f64, value: Option<f64>| *slot = value.unwrap_or(0.0);

    set(&mut t.rpm, r.telemetry().map(|c| u16_at(c, car_telemetry::ENGINE_RPM)));
    set(&mut t.max_rpm, r.status().map(|c| u16_at(c, car_status::MAX_RPM)));
    set(&mut t.gear, r.telemetry().map(|c| f64::from(i8_at(c, car_telemetry::GEAR))));
    set(&mut t.speed, r.telemetry().and_then(|c| python_int(u16_at(c, car_telemetry::SPEED) * spec.speed_factor)));
    let team = r.car(packet_id::PARTICIPANTS, spec.participants).and_then(|c| lookup(spec.teams, c[spec.team_id_at]));
    t.vehicle = team.unwrap_or_default().to_string();
    let track = r.store.get(packet_id::SESSION).and_then(|s| lookup(spec.tracks, i8_at(s, spec.session_track_id_at)));
    t.location = track.unwrap_or_default().to_string();

    let wheels = spec.wheel_data;
    for wheel in WHEELS {
        let (travel, slip, velocity, rumble) = wheel.channels(t);
        *travel = r.wheel(wheels.suspension_position_at, wheel).map_or(0.0, |p| p / 10000.0);
        *slip = r.wheel(wheels.wheel_slip_at, wheel).map_or(0.0, |s| (s * 10.0).abs());
        *velocity = r.wheel(wheels.suspension_velocity_at, wheel).unwrap_or(0.0);
        *rumble = r.telemetry().map_or(0.0, |c| rumble_strength(c[spec.surface_type_at + wheel.index()]));
    }

    set(&mut t.steering_wheel_angle, r.telemetry().map(|c| f32_at(c, car_telemetry::STEER) * 10.0));
    set(&mut t.vertical_acceleration, r.motion(car_motion::G_FORCE_VERTICAL));
    set(&mut t.lateral_acceleration, r.motion(car_motion::G_FORCE_LATERAL));
    set(&mut t.longitudinal_acceleration, r.motion(car_motion::G_FORCE_LONGITUDINAL).map(|g| g * 10.0));
    set(&mut t.yaw, r.motion(car_motion::YAW).map(radians_to_degrees));
    set(&mut t.roll, r.motion(car_motion::ROLL).map(radians_to_degrees));
    let pitch_sign = if spec.invert_pitch { -1.0 } else { 1.0 };
    set(&mut t.pitch, r.motion(car_motion::PITCH).map(|p| radians_to_degrees(p) * pitch_sign));
    set(&mut t.vertical_velocity, r.local_velocity(1));
    set(&mut t.lateral_velocity, r.local_velocity(0).map(|v| -v));
    let longitudinal_sign = if spec.invert_longitudinal_velocity { -1.0 } else { 1.0 };
    set(&mut t.longitudinal_velocity, r.local_velocity(2).map(|v| v * longitudinal_sign));

    let flags = r.status().map(|c| i8_at(c, spec.fia_flags_at));
    set(&mut t.flag, flags.map(fia_flag_code));
    set(&mut t.raw_flag, flags.map(f64::from));
    let fuel = r.status().and_then(|c| {
        let capacity = f32_at(c, car_status::FUEL_CAPACITY);
        (capacity != 0.0).then(|| f32_at(c, car_status::FUEL_IN_TANK) / capacity * 100.0)
    });
    set(&mut t.fuel_level, fuel);
}

/// An F1 2019–2025 listener with its receiver thread.
pub struct F1SeasonSource {
    shared: Arc<Shared>,
    receiver: Option<JoinHandle<()>>,
}

impl F1SeasonSource {
    /// Start the receiver on `0.0.0.0:<settings.port>` (bound once).
    pub fn new(season: Season, settings: &ListenerSettings) -> Self {
        let socket = UdpEndpoint::new(Endpoint::any_interface(settings.port, TIMEOUT));
        F1SeasonSource::with_socket(season, Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Keep the configured port as an integer until the receiver creates its socket and binds it.
    pub fn from_imported_port(season: Season, port: ImportedPort) -> Self {
        let socket = UdpEndpoint::from_imported_port(Ipv4Addr::UNSPECIFIED, port, TIMEOUT);
        F1SeasonSource::with_socket(season, Box::new(socket), Arc::new(Forwarder::default()))
    }

    /// Start the receiver on the given socket (tests).
    pub fn with_socket(season: Season, socket: Box<dyn DatagramSocket>, forwarder: Arc<Forwarder>) -> Self {
        let shared = Arc::new(Shared::new(season.spec(), forwarder));
        let thread_shared = Arc::clone(&shared);
        let receiver = std::thread::Builder::new()
            .name(format!("{}_main_loop", season.id()))
            .spawn(move || thread_shared.run(socket.as_ref()))
            .map_err(|error| tracing::warn!("{}: cannot start the receiver: {error}", season.id()))
            .ok();
        F1SeasonSource { shared, receiver }
    }
}

impl Drop for F1SeasonSource {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.join();
        }
    }
}

impl Source for F1SeasonSource {
    fn id(&self) -> &'static str {
        self.shared.spec.id
    }

    /// The liveness test: the connected flag.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.shared.connected.load(Ordering::Relaxed))
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.shared.connected.load(Ordering::Relaxed))
    }

    /// A channel read is atomic with respect to the receiver, which cannot run
    /// inside it: the whole call reads one state of the packet store and the player car index.
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let spec = self.shared.spec;
        let value = {
            let mut store = self.shared.store();
            let Some(first) = store.first() else { return Some(LegacyValue::Int(0)) };
            let initial_player = i64::from(first[spec.header.player_car_at]);
            store.player_car = initial_player;
            let store = &*store;
            let packet = |id| store.packets.iter().find(|(key, _)| *key == id).map(|(_, p)| Arc::clone(p));
            NativeReader { spec, initial_player, packet, player: || store.player_car }.native_value(name)
        };
        // F1 23 alone prints a failed lateral-velocity read after "motion";
        // its only failure is a missing Motion Ex packet. The printing
        // happens with the packets released.
        if value.is_none() && spec.packet_format == 2023 && name == "lateral_velocity" {
            crate::printed::line("motion <PacketID.MOTION_EX: 13>");
        }
        Some(value.unwrap_or(LegacyValue::Int(0)))
    }

    /// The receive step (the connected flag) and the channel reads from the stored packets.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let connected = self.shared.connected.load(Ordering::Relaxed);
        publish(self.shared.spec, &mut self.shared.store(), telemetry);
        Ok(connected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::ea_f1::layout::{F1_2019, F1_2020, F1_2021, F1_2022, F1_2023, F1_2024, F1_2025};
    use crate::udp_racing::socket::scripted::ScriptedSocket;

    #[test]
    fn native_season_setup_failure_ends_worker_before_receive_or_publication() {
        use crate::udp_racing::forward::PortForwarding;
        use crate::udp_racing::parameters::Parameters;
        use crate::udp_racing::socket::socket_creation_error;
        use std::sync::atomic::AtomicUsize;

        struct FailedSetup {
            creation: bool,
            attempts: AtomicUsize,
        }
        impl DatagramSocket for FailedSetup {
            fn reopen(&self) -> io::Result<()> {
                self.attempts.fetch_add(1, Ordering::Relaxed);
                let error = io::Error::new(io::ErrorKind::InvalidInput, "scripted setup failure");
                Err(if self.creation { socket_creation_error(error) } else { error })
            }
            fn close(&self) {
                panic!("the source has no explicit close call after failed setup")
            }
            fn receive(&self, _: usize) -> io::Result<Vec<u8>> {
                panic!("setup failure ends the worker before receive")
            }
        }

        for season in
            [Season::Y2019, Season::Y2020, Season::Y2021, Season::Y2022, Season::Y2023, Season::Y2024, Season::Y2025]
        {
            for creation in [false, true] {
                let parameters = Arc::new(Parameters::default());
                parameters.set_listener_running(season.id());
                parameters.add_game_error("earlier error".into());
                let forwarding = Arc::new(PortForwarding::default());
                forwarding.set_reader(Arc::new(|_| panic!("setup failure does not forward a datagram")));
                let shared =
                    Arc::new(Shared::new(season.spec(), Arc::new(Forwarder::with(parameters.clone(), forwarding))));
                let socket = FailedSetup { creation, attempts: AtomicUsize::new(0) };
                shared.run(&socket);
                assert_eq!(socket.attempts.load(Ordering::Relaxed), 1, "{} setup is not retried", season.id());
                assert!(!shared.connected.load(Ordering::Relaxed));
                assert!(shared.store().packets.is_empty());
                assert_eq!(shared.store().player_car, -1);
                assert_eq!(parameters.game_errors(), ["earlier error".into()].into());
                let mut source = F1SeasonSource { shared: shared.clone(), receiver: None };
                assert!(!source.is_running().unwrap());
                let mut telemetry = Telemetry::off();
                telemetry.rpm = 1234.0;
                assert!(!source.receive_native(&mut telemetry).unwrap());
                assert_eq!(telemetry.rpm, 1234.0, "receiveData only returns connected");
                assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(0)));
                assert_eq!(socket.attempts.load(Ordering::Relaxed), 1, "dispatcher does not restart the receiver");

                // A packet already in the store shows whether a failed socket setup would reset the store.
                let packet = Builder(season.spec()).packet(packet_id::MOTION, 2);
                shared.store().insert(packet_id::MOTION, packet.clone());
                shared.store().player_car = 2;
                shared.run(&socket);
                assert_eq!(shared.store().get(packet_id::MOTION), Some(packet.as_slice()));
                assert_eq!(shared.store().player_car, 2);
            }
        }
    }

    #[test]
    fn native_receive_does_not_read_the_player_header() {
        let shared = Arc::new(Shared::new(&F1_2022, Arc::new(Forwarder::default())));
        shared.store().insert(packet_id::MOTION, Builder(&F1_2022).packet(packet_id::MOTION, 4));
        let mut source = F1SeasonSource { shared, receiver: None };
        source.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(source.shared.store().player_car, -1);
        assert_eq!(source.native_channel("unknown"), Some(LegacyValue::Int(0)));
        assert_eq!(source.shared.store().player_car, 4);
    }

    #[test]
    fn native_fuel_retains_each_packet_but_rereads_packet_and_player_for_the_denominator() {
        use std::cell::{Cell, RefCell};
        for spec in [&F1_2019, &F1_2020, &F1_2021, &F1_2022, &F1_2023, &F1_2024, &F1_2025] {
            let b = Builder(spec);
            let mut first = b.packet(packet_id::CAR_STATUS, 2);
            let at = b.car_offset(spec.status, 2);
            put_f32(&mut first, at + car_status::FUEL_IN_TANK, 20.0);
            put_f32(&mut first, at + car_status::FUEL_CAPACITY, 100.0);
            let mut newer = b.packet(packet_id::CAR_STATUS, 3);
            for car in [2, 3] {
                let at = b.car_offset(spec.status, car);
                put_f32(&mut newer, at + car_status::FUEL_IN_TANK, 30.0);
                put_f32(&mut newer, at + car_status::FUEL_CAPACITY, 50.0);
            }
            let first: Arc<[u8]> = first.into();
            let newer: Arc<[u8]> = newer.into();
            let trace = RefCell::new(Vec::new());
            let count = Cell::new(0);
            let packet = |id| {
                assert_eq!(id, packet_id::CAR_STATUS);
                trace.borrow_mut().push("packet");
                count.set(count.get() + 1);
                Some(if count.get() == 1 { first.clone() } else { newer.clone() })
            };
            let player = || {
                trace.borrow_mut().push("player");
                if count.get() == 1 {
                    2
                } else {
                    3
                }
            };
            let mut reader = NativeReader { spec, initial_player: 2, packet, player };
            assert_eq!(reader.native_value("fuel_level"), Some(LegacyValue::Float(40.0)), "{}", spec.id);
            if spec.packet_format >= 2022 {
                assert_eq!(*trace.borrow(), ["packet", "player", "packet", "player"]);
            } else {
                assert_eq!(*trace.borrow(), ["packet", "packet"]);
            }

            // A second status lookup that finds no packet, or a zero fuel capacity, fails the read; the first
            // packet is not reused.
            for next in [None, Some(b.packet(packet_id::CAR_STATUS, 2).into())] {
                let calls = Cell::new(0);
                let packet = |_| {
                    calls.set(calls.get() + 1);
                    if calls.get() == 1 {
                        Some(first.clone())
                    } else {
                        next.clone()
                    }
                };
                let mut reader = NativeReader { spec, initial_player: 2, packet, player: || 2 };
                assert_eq!(reader.native_value("fuel_level").unwrap_or(LegacyValue::Int(0)), LegacyValue::Int(0));
                assert_eq!(calls.get(), 2);
            }
        }
    }

    #[test]
    fn native_packet_lookup_retains_the_packet_before_reading_current_player() {
        for spec in [&F1_2019, &F1_2020, &F1_2021, &F1_2022, &F1_2023, &F1_2024, &F1_2025] {
            let b = Builder(spec);
            let mut packet = b.packet(packet_id::CAR_TELEMETRY, 2);
            put_u16(&mut packet, b.car_offset(spec.telemetry, 2) + car_telemetry::ENGINE_RPM, 7222);
            put_u16(&mut packet, b.car_offset(spec.telemetry, 3) + car_telemetry::ENGINE_RPM, 7333);
            let packet: Arc<[u8]> = packet.into();
            let shared = Shared::new(spec, Arc::new(Forwarder::default()));
            shared.store().insert(packet_id::CAR_TELEMETRY, packet.to_vec());
            let get = |id| {
                assert_eq!(id, packet_id::CAR_TELEMETRY);
                let retained = shared.store().packets[0].1.clone();
                shared.store().packets.clear();
                Some(retained)
            };
            let player = || {
                assert!(shared.store().packets.is_empty());
                3
            };
            let mut reader = NativeReader { spec, initial_player: 2, packet: get, player };
            assert_eq!(
                reader.native_value("rpm"),
                Some(LegacyValue::Int(if spec.packet_format >= 2022 { 7333 } else { 7222 }))
            );

            let mut reader = NativeReader {
                spec,
                initial_player: 2,
                packet: |_| panic!("an unknown channel has no packet expression"),
                player: || panic!("an unknown channel has no player subscript"),
            };
            assert_eq!(reader.native_value("unknown"), Some(LegacyValue::Int(0)));

            let session: Arc<[u8]> = b.packet(packet_id::SESSION, 2).into();
            let mut reader = NativeReader {
                spec,
                initial_player: 2,
                packet: |id| {
                    assert_eq!(id, packet_id::SESSION);
                    Some(session.clone())
                },
                player: || panic!("a session field has no player subscript"),
            };
            let _ = reader.native_value("track_loc");
        }
    }

    /// F1 23 prints the missing Motion Ex packet of its lateral-velocity getter; the other seasons and
    /// the other getters of that packet only return 0.
    #[test]
    fn f1_2023_prints_the_missing_motion_ex_packet_of_lateral_velocity() {
        for season in
            [Season::Y2019, Season::Y2020, Season::Y2021, Season::Y2022, Season::Y2023, Season::Y2024, Season::Y2025]
        {
            let spec = season.spec();
            let shared = Arc::new(Shared::new(spec, Arc::new(Forwarder::default())));
            shared.store().insert(packet_id::CAR_TELEMETRY, Builder(spec).packet(packet_id::CAR_TELEMETRY, 0));
            let source = F1SeasonSource { shared, receiver: None };
            crate::printed::take();
            for name in ["vertical_velocity", "lateral_velocity", "longitudinal_velocity", "wheel_slip_lf"] {
                assert_eq!(source.native_channel(name), Some(LegacyValue::Int(0)), "{} {name}", spec.id);
            }
            let expected: &[&str] = if spec.packet_format == 2023 { &["motion <PacketID.MOTION_EX: 13>"] } else { &[] };
            assert_eq!(crate::printed::take(), expected, "{}", spec.id);
        }
        assert_eq!(F1_2023.wheel_data.packet_id, 13);
        let shared = Arc::new(Shared::new(&F1_2023, Arc::new(Forwarder::default())));
        shared.store().insert(13, Builder(&F1_2023).packet(13, 0));
        let source = F1SeasonSource { shared, receiver: None };
        assert_eq!(source.native_channel("lateral_velocity"), Some(LegacyValue::Float(-0.0)));
        assert!(crate::printed::take().is_empty());
        // An empty packet store ends the getter before any channel: nothing is printed.
        let source =
            F1SeasonSource { shared: Arc::new(Shared::new(&F1_2023, Arc::new(Forwarder::default()))), receiver: None };
        assert_eq!(source.native_channel("lateral_velocity"), Some(LegacyValue::Int(0)));
        assert!(crate::printed::take().is_empty());
    }

    #[test]
    fn native_season_kinds_and_live_packets_reach_the_adapter() {
        use crate::derive::{Listener, SourceListener};
        for season in
            [Season::Y2019, Season::Y2020, Season::Y2021, Season::Y2022, Season::Y2023, Season::Y2024, Season::Y2025]
        {
            let spec = season.spec();
            let b = Builder(spec);
            let shared = Arc::new(Shared::new(spec, Arc::new(Forwarder::default())));
            let mut telemetry = b.packet(packet_id::CAR_TELEMETRY, 2);
            let at = b.car_offset(spec.telemetry, 2);
            put_u16(&mut telemetry, at + car_telemetry::ENGINE_RPM, 9000);
            telemetry[at + car_telemetry::GEAR] = 2;
            telemetry[at + spec.surface_type_at + Wheel::FrontLeft.index()] = 1;
            let mut status = b.packet(packet_id::CAR_STATUS, 2);
            let at = b.car_offset(spec.status, 2);
            put_u16(&mut status, at + car_status::MAX_RPM, 12000);
            put_f32(&mut status, at + car_status::FUEL_CAPACITY, 100.0);
            status[at + spec.fia_flags_at] = 1;
            shared.store().insert(packet_id::CAR_TELEMETRY, telemetry.clone());
            shared.store().insert(packet_id::CAR_STATUS, status.clone());
            shared.store().insert(packet_id::MOTION, b.packet(packet_id::MOTION, 2));
            let source = F1SeasonSource { shared: Arc::clone(&shared), receiver: None };
            let mut listener = SourceListener::new(source);
            listener.receive();
            assert_eq!(shared.store().player_car, -1);
            for (name, expected) in [
                ("rpm", LegacyValue::Int(9000)),
                ("max_rpm", LegacyValue::Int(12000)),
                ("gear", LegacyValue::Int(2)),
                ("speed", LegacyValue::Int(0)),
                ("flag", LegacyValue::Int(12)),
                ("raw_flag", LegacyValue::Int(1)),
                ("rumble_pitch_lf", LegacyValue::Int(40)),
                ("fuel_level", LegacyValue::Float(0.0)),
                ("vertical_acceleration", LegacyValue::Float(0.0)),
                ("steering_wheel_angle", LegacyValue::Float(0.0)),
                ("car_name", LegacyValue::Int(0)),
                ("track_loc", LegacyValue::Int(0)),
                ("unknown", LegacyValue::Int(0)),
            ] {
                assert_eq!(listener.native_channel(name), Some(expected), "{} {name}", spec.id);
            }
            assert_eq!(shared.store().player_car, 2);
            let at = b.car_offset(spec.telemetry, 2);
            put_u16(&mut telemetry, at + car_telemetry::ENGINE_RPM, 7777);
            shared.store().insert(packet_id::CAR_TELEMETRY, telemetry);
            assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Int(7777)));
            let at = b.car_offset(spec.status, 2);
            put_f32(&mut status, at + car_status::FUEL_CAPACITY, 0.0);
            shared.store().insert(packet_id::CAR_STATUS, status);
            assert_eq!(listener.native_channel("fuel_level"), Some(LegacyValue::Int(0)));
            shared.store().packets.clear();
            assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Int(0)));
            assert_eq!(shared.store().player_car, 2, "an empty dictionary does not assign player_car");
        }
    }

    /// Builds packets of one season per its layout.
    struct Builder(&'static SeasonSpec);

    impl Builder {
        fn packet(&self, id: u8, player: u8) -> Vec<u8> {
            let size = self.0.packet_sizes.iter().find(|(known, _)| *known == id).unwrap().1;
            let mut bytes = vec![0u8; size];
            bytes[..2].copy_from_slice(&self.0.packet_format.to_le_bytes());
            bytes[self.0.header.packet_version_at] = 1;
            bytes[self.0.header.packet_id_at] = id;
            bytes[self.0.header.player_car_at] = player;
            bytes
        }

        fn car_offset(&self, array: CarArray, car: usize) -> usize {
            array.at + car * array.stride
        }
    }

    fn put_f32(bytes: &mut [u8], at: usize, value: f32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u16(bytes: &mut [u8], at: usize, value: u16) {
        bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn telemetry_of(spec: &'static SeasonSpec, packets: &[Vec<u8>]) -> Telemetry {
        let shared = Shared::new(spec, Arc::new(Forwarder::default()));
        for packet in packets {
            shared.on_receive(Ok(packet.clone()));
        }
        let mut t = Telemetry::off();
        publish(spec, &mut shared.store(), &mut t);
        t
    }

    #[test]
    fn empty_store_reads_zero_for_every_channel() {
        // An empty packet store has no first packet -> every channel reads 0.
        let t = telemetry_of(&F1_2019, &[]);
        assert_eq!((t.gear, t.max_rpm, t.rpm, t.steering_wheel_angle), (0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn f1_2019_car_telemetry_and_status_channels() {
        // F1 2019 channels: rpm, max_rpm, gear, speed * 0.6214, steer * 10, rumble 40, flag map, fuel %.
        let b = Builder(&F1_2019);
        let player = 3;
        let mut telemetry = b.packet(packet_id::CAR_TELEMETRY, player as u8);
        let at = b.car_offset(F1_2019.telemetry, player);
        put_u16(&mut telemetry, at, 200);
        put_f32(&mut telemetry, at + 6, -0.5);
        telemetry[at + 15] = (-1i8) as u8;
        put_u16(&mut telemetry, at + 16, 11_850);
        telemetry[at + 62 + 2] = 1; // FL on a rumble strip -> rumble_pitch_lf
        let mut status = b.packet(packet_id::CAR_STATUS, player as u8);
        let at = b.car_offset(F1_2019.status, player);
        put_f32(&mut status, at + 5, 25.0);
        put_f32(&mut status, at + 9, 100.0);
        put_u16(&mut status, at + 17, 12_000);
        status[at + 38] = 3; // yellow
        let t = telemetry_of(&F1_2019, &[telemetry, status]);
        assert_eq!((t.rpm, t.max_rpm, t.gear), (11_850.0, 12_000.0, -1.0));
        assert_eq!(t.speed, 124.0); // 200 * 0.6214 = 124.28, truncated
        assert_eq!(t.steering_wheel_angle, -5.0);
        assert_eq!((t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr), (40.0, 0.0, 0.0, 0.0));
        assert_eq!((t.flag, t.raw_flag, t.fuel_level), (6.0, 3.0, 25.0));
        // Unmapped channels read 0.
        assert_eq!(t.tyre_temperature_lf, 0.0);
    }

    #[test]
    fn f1_2019_motion_channels() {
        // MOTION: player gForce/yaw/pitch/roll plus wheel arrays [RL, RR, FL, FR] and local velocity.
        let b = Builder(&F1_2019);
        let mut motion = b.packet(packet_id::MOTION, 1);
        let car = F1_2019.motion_cars_at + car_motion::STRIDE;
        put_f32(&mut motion, car + 36, 1.5);
        put_f32(&mut motion, car + 40, -0.5);
        put_f32(&mut motion, car + 44, 0.25);
        put_f32(&mut motion, car + 48, 1.0);
        put_f32(&mut motion, car + 52, 0.5);
        let wheels = F1_2019.wheel_data;
        put_f32(&mut motion, wheels.suspension_position_at + 8, 120.0); // FL
        put_f32(&mut motion, wheels.wheel_slip_at, -0.3); // RL
        put_f32(&mut motion, wheels.suspension_velocity_at + 12, 2.5); // FR
        put_f32(&mut motion, wheels.local_velocity_at, 1.0);
        put_f32(&mut motion, wheels.local_velocity_at + 4, 2.0);
        put_f32(&mut motion, wheels.local_velocity_at + 8, 3.0);
        let t = telemetry_of(&F1_2019, &[motion]);
        assert_eq!((t.lateral_acceleration, t.longitudinal_acceleration, t.vertical_acceleration), (1.5, -5.0, 0.25));
        assert_eq!(t.yaw, radians_to_degrees(1.0));
        assert_eq!(t.pitch, radians_to_degrees(0.5));
        assert_eq!(t.suspension_travel_lf, 120.0 / 10000.0);
        assert_eq!(t.wheel_slip_lr, (f64::from(-0.3f32) * 10.0).abs());
        assert_eq!(t.suspension_velocity_rf, 2.5);
        assert_eq!((t.lateral_velocity, t.vertical_velocity, t.longitudinal_velocity), (-1.0, 2.0, 3.0));
    }

    #[test]
    fn sign_changes_of_later_seasons() {
        // F1 22: pitch * -1; F1 23: also localVelocityZ * -1, wheel data from MOTION_EX, speed * 0.27778.
        let b = Builder(&F1_2023);
        let mut motion = b.packet(packet_id::MOTION, 0);
        put_f32(&mut motion, F1_2023.motion_cars_at + 52, 0.5);
        let mut extra = b.packet(packet_id::MOTION_EX, 0);
        put_f32(&mut extra, 161 + 8, 3.0);
        put_f32(&mut extra, 29 + 12, 50.0); // FR suspension position
        let mut telemetry = b.packet(packet_id::CAR_TELEMETRY, 0);
        put_u16(&mut telemetry, F1_2023.telemetry.at, 100);
        let t = telemetry_of(&F1_2023, &[motion, extra, telemetry]);
        assert_eq!(t.pitch, -radians_to_degrees(0.5));
        assert_eq!(t.longitudinal_velocity, -3.0);
        assert_eq!(t.suspension_travel_rf, 50.0 / 10000.0);
        assert_eq!(t.speed, 27.0); // 100 * 0.27778, truncated
    }

    #[test]
    fn player_car_comes_from_the_first_stored_packet_type() {
        // The player car index comes from the first stored packet; store order is first insertion.
        let b = Builder(&F1_2019);
        let first = b.packet(packet_id::SESSION, 5);
        let mut telemetry = b.packet(packet_id::CAR_TELEMETRY, 0);
        put_u16(&mut telemetry, b.car_offset(F1_2019.telemetry, 5) + 16, 9000);
        let t = telemetry_of(&F1_2019, &[first.clone(), telemetry.clone()]);
        assert_eq!(t.rpm, 9000.0);
        // Re-sending the session packet keeps its place; player stays car 5.
        let t = telemetry_of(&F1_2019, &[first.clone(), telemetry.clone(), b.packet(packet_id::SESSION, 5)]);
        assert_eq!(t.rpm, 9000.0);
        // A first packet naming car 255 makes every per-car lookup fail (out of range) -> 0.
        let t = telemetry_of(&F1_2019, &[b.packet(packet_id::SESSION, 255), telemetry]);
        assert_eq!(t.rpm, 0.0);
    }

    #[test]
    fn names_come_from_team_and_track_tables() {
        let b = Builder(&F1_2019);
        let mut participants = b.packet(packet_id::PARTICIPANTS, 0);
        participants[F1_2019.participants.at + 2] = 9;
        let mut session = b.packet(packet_id::SESSION, 0);
        session[F1_2019.session_track_id_at] = 10;
        let t = telemetry_of(&F1_2019, &[participants, session]);
        assert_eq!((t.vehicle.as_str(), t.location.as_str()), ("Alfa Romeo", "Spa"));
    }

    #[test]
    fn receiver_connection_rules_by_season() {
        // F1 2019 receiver: connected for any datagram (even a rejected one); not connected on timeout.
        let shared = Shared::new(&F1_2019, Arc::new(Forwarder::default()));
        shared.on_receive(Ok(vec![1, 2, 3]));
        assert!(shared.connected.load(Ordering::Relaxed));
        assert!(shared.store().packets.is_empty());
        shared.on_receive(Ok(Builder(&F1_2019).packet(packet_id::MOTION, 0)));
        shared.on_receive(Err(io::Error::from(io::ErrorKind::WouldBlock)));
        assert!(!shared.connected.load(Ordering::Relaxed));
        assert_eq!(shared.store().packets.len(), 1, "2019 keeps cfd across timeouts");

        // F1 22: connected only when the player's team id is in the team table; the player car index starts at -1.
        let b = Builder(&F1_2022);
        let shared = Shared::new(&F1_2022, Arc::new(Forwarder::default()));
        let mut participants = b.packet(packet_id::PARTICIPANTS, 0);
        participants[F1_2022.participants.at + 21 * F1_2022.participants.stride + 3] = 8; // car -1 = 21: McLaren
        shared.on_receive(Ok(participants.clone()));
        assert!(!shared.connected.load(Ordering::Relaxed), "the check runs before this datagram is stored");
        shared.on_receive(Ok(b.packet(packet_id::MOTION, 0)));
        assert!(shared.connected.load(Ordering::Relaxed));
        participants[F1_2022.participants.at + 21 * F1_2022.participants.stride + 3] = 200; // not in the team table
        shared.on_receive(Ok(participants));
        shared.on_receive(Ok(b.packet(packet_id::MOTION, 0)));
        assert!(!shared.connected.load(Ordering::Relaxed));
        shared.on_receive(Err(io::Error::from(io::ErrorKind::WouldBlock)));
        assert!(shared.store().packets.is_empty(), "2021+ reset cfd on a failed receive");
    }

    #[test]
    fn get_channel_updates_the_player_car_used_by_the_receiver() {
        let b = Builder(&F1_2022);
        let shared = Shared::new(&F1_2022, Arc::new(Forwarder::default()));
        let mut participants = b.packet(packet_id::PARTICIPANTS, 4);
        participants[F1_2022.participants.at + 4 * F1_2022.participants.stride + 3] = 0;
        participants[F1_2022.participants.at + 21 * F1_2022.participants.stride + 3] = 200;
        shared.on_receive(Ok(participants));
        shared.on_receive(Ok(b.packet(packet_id::MOTION, 4)));
        assert!(!shared.connected.load(Ordering::Relaxed));
        publish(&F1_2022, &mut shared.store(), &mut Telemetry::off());
        shared.on_receive(Ok(b.packet(packet_id::MOTION, 4)));
        assert!(shared.connected.load(Ordering::Relaxed));
    }

    /// Offsets of one season's byte-packed packet fields, stated independently of
    /// `layout.rs`: header, then `(array start, stride)` of the per-car arrays and the fields inside them.
    struct OriginalLayout {
        spec: &'static SeasonSpec,
        format: u16,
        version_at: usize,
        id_at: usize,
        player_at: usize,
        telemetry_size: usize,
        telemetry: (usize, usize),
        surface_type_at: usize,
        status_size: usize,
        status: (usize, usize),
        fia_flags_at: usize,
        participants_size: usize,
        participants: (usize, usize),
        team_id_at: usize,
        session_size: usize,
        track_id_at: usize,
        team: (u8, &'static str),
        track: (i8, &'static str),
    }

    const ORIGINAL_LAYOUTS: [OriginalLayout; 4] = [
        OriginalLayout {
            spec: &F1_2020,
            format: 2020,
            version_at: 4,
            id_at: 5,
            player_at: 22,
            telemetry_size: 1307,
            telemetry: (24, 58),
            surface_type_at: 54,
            status_size: 1344,
            status: (24, 60),
            fia_flags_at: 42,
            participants_size: 1213,
            participants: (25, 54),
            team_id_at: 2,
            session_size: 251,
            track_id_at: 31,
            team: (4, "Racing Point"),
            track: (25, "Hanoi"),
        },
        OriginalLayout {
            spec: &F1_2021,
            format: 2021,
            version_at: 4,
            id_at: 5,
            player_at: 22,
            telemetry_size: 1347,
            telemetry: (24, 60),
            surface_type_at: 56,
            status_size: 1058,
            status: (24, 47),
            fia_flags_at: 28,
            participants_size: 1257,
            participants: (25, 56),
            team_id_at: 3,
            session_size: 625,
            track_id_at: 31,
            team: (5, "Alpine"),
            track: (28, "Portimão"),
        },
        OriginalLayout {
            spec: &F1_2024,
            format: 2024,
            version_at: 5,
            id_at: 6,
            player_at: 27,
            telemetry_size: 1352,
            telemetry: (29, 60),
            surface_type_at: 56,
            status_size: 1239,
            status: (29, 55),
            fia_flags_at: 28,
            participants_size: 1350,
            participants: (30, 60),
            team_id_at: 3,
            session_size: 753,
            track_id_at: 36,
            team: (9, "Sauber"),
            track: (31, "Las Vegas"),
        },
        OriginalLayout {
            spec: &F1_2025,
            format: 2025,
            version_at: 5,
            id_at: 6,
            player_at: 27,
            telemetry_size: 1352,
            telemetry: (29, 60),
            surface_type_at: 56,
            status_size: 1239,
            status: (29, 55),
            fia_flags_at: 28,
            participants_size: 1284,
            participants: (30, 57),
            team_id_at: 3,
            session_size: 753,
            track_id_at: 36,
            team: (6, "RB"),
            track: (32, "Losail"),
        },
    ];

    impl OriginalLayout {
        fn packet(&self, id: u8, size: usize, player: u8) -> Vec<u8> {
            let mut bytes = vec![0u8; size];
            bytes[..2].copy_from_slice(&self.format.to_le_bytes());
            bytes[self.version_at] = 1;
            bytes[self.id_at] = id;
            bytes[self.player_at] = player;
            bytes
        }
    }

    #[test]
    fn per_season_offsets_of_2020_2021_2024_and_2025() {
        // F1 2020–2025 channels: engineRPM, maxRPM, vehicleFiaFlags (green 1 -> 12),
        // fuelInTank / fuelCapacity * 100, surfaceType[wheel] == 1 -> 40, the team name, the track name.
        const PLAYER: usize = 2;
        for layout in &ORIGINAL_LAYOUTS {
            let car = |(at, stride): (usize, usize)| at + PLAYER * stride;
            let mut telemetry = layout.packet(packet_id::CAR_TELEMETRY, layout.telemetry_size, PLAYER as u8);
            let at = car(layout.telemetry);
            put_u16(&mut telemetry, at + 16, 10_500);
            telemetry[at + layout.surface_type_at + 3] = 1; // FR on a rumble strip
            let mut status = layout.packet(packet_id::CAR_STATUS, layout.status_size, PLAYER as u8);
            let at = car(layout.status);
            put_f32(&mut status, at + 5, 30.0);
            put_f32(&mut status, at + 9, 120.0);
            put_u16(&mut status, at + 17, 13_000);
            status[at + layout.fia_flags_at] = 1;
            let mut participants = layout.packet(packet_id::PARTICIPANTS, layout.participants_size, PLAYER as u8);
            participants[car(layout.participants) + layout.team_id_at] = layout.team.0;
            let mut session = layout.packet(packet_id::SESSION, layout.session_size, PLAYER as u8);
            session[layout.track_id_at] = layout.track.0 as u8;

            let t = telemetry_of(layout.spec, &[telemetry, status, participants, session]);
            let season = layout.spec.id;
            assert_eq!((t.rpm, t.max_rpm), (10_500.0, 13_000.0), "{season}");
            assert_eq!((t.flag, t.raw_flag), (12.0, 1.0), "{season}");
            assert_eq!(t.fuel_level, 25.0, "{season}");
            assert_eq!(
                (t.rumble_pitch_rf, t.rumble_pitch_lf, t.rumble_pitch_lr, t.rumble_pitch_rr),
                (40.0, 0.0, 0.0, 0.0),
                "{season}"
            );
            assert_eq!((t.vehicle.as_str(), t.location.as_str()), (layout.team.1, layout.track.1), "{season}");
        }
    }

    #[test]
    fn f1_2021_receiver_connects_on_any_datagram_and_forgets_packets_on_failure() {
        // F1 2021 receiver: connected after every datagram (no team check before 2022);
        // a failed receive resets the packet store and clears the connected flag.
        let layout = &ORIGINAL_LAYOUTS[1];
        let shared = Shared::new(&F1_2021, Arc::new(Forwarder::default()));
        shared.on_receive(Ok(vec![0u8; 3]));
        assert!(shared.connected.load(Ordering::Relaxed));
        let mut participants = layout.packet(packet_id::PARTICIPANTS, layout.participants_size, 0);
        participants[layout.participants.0 + 21 * layout.participants.1 + layout.team_id_at] = 200; // unknown team
        shared.on_receive(Ok(participants));
        shared.on_receive(Ok(layout.packet(packet_id::SESSION, layout.session_size, 0)));
        assert!(shared.connected.load(Ordering::Relaxed), "2021 ignores the team id");
        assert_eq!(shared.store().packets.len(), 2);
        shared.on_receive(Err(io::Error::from(io::ErrorKind::WouldBlock)));
        assert!(!shared.connected.load(Ordering::Relaxed));
        assert!(shared.store().packets.is_empty());
    }

    #[test]
    fn receiver_thread_feeds_the_source() {
        let socket = ScriptedSocket::new();
        let b = Builder(&F1_2019);
        let mut telemetry = b.packet(packet_id::CAR_TELEMETRY, 0);
        put_u16(&mut telemetry, F1_2019.telemetry.at + 16, 4000);
        socket.push(&telemetry);
        let mut source =
            F1SeasonSource::with_socket(Season::Y2019, Box::new(socket.clone()), Arc::new(Forwarder::default()));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while socket.pending() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(20));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 4000.0);
        assert_eq!(source.id(), "f12019");
        drop(source);
        assert_eq!(socket.activity().receives[0], 2048);
    }
}

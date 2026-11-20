//! Project CARS on PlayStation 4 / Xbox One, "UDP = 1" protocol (listener `projectcarsudp`).
//!
//! Datagrams follow the game's UDP protocol: a 3-byte header whose low two bits of byte 2 give the packet
//! type (0 telemetry, 1 participant strings, 2 additional strings).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::forward::Forwarder;
pub use super::socket::ListenerSettings;
use super::socket::{socket_creation_failed, ImportedPort};
use super::support::channels::zero_listener_channels;
use super::support::net::{Datagrams, Endpoint, Network, SystemNetwork};
use super::support::pymath::rad_to_deg;
use super::support::wire::{decode_latin1, Fields, Short};
use crate::{Source, SourceError};

/// (The configuration default, which tests bind; the listener itself has none: it reads `NETWORK_PORT` at construction.)
#[cfg(test)]
const DEFAULT_PORT: u16 = 5606;
const RECEIVE_SIZE: usize = 2400;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);

const TELEMETRY_LEN: usize = 1367;
const STRINGS_LEN: usize = 1347;
const ADDITIONAL_STRINGS_LEN: usize = 1028;
const NAME_LEN: usize = 64;
const PARTICIPANTS: usize = 56;
/// Terrain codes reported as a rumble strip (TERRAIN_RUMBLE_STRIPS, TERRAIN_EXIT_RUMBLE_STRIPS, 40, 41).
const RUMBLE_TERRAIN: [u8; 4] = [10, 25, 40, 41];

/// The values the receive step copies out of a telemetry packet.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Motion {
    max_rpm: f64,
    rpm: f64,
    gear: f64,
    speed: f64,
    steering: f64,
    orientation: [f64; 3],
    local_velocity: [f64; 3],
    local_acceleration: [f64; 3],
    /// Front-left, front-right, rear-left, rear-right.
    terrain: [u8; 4],
}

/// A decoded datagram.
#[derive(Debug, Clone, PartialEq)]
enum Packet {
    Telemetry(Motion),
    Strings { car_name: String },
    AdditionalStrings,
    Other,
}

/// The text of a 64-byte name up to its first NUL; a name without one is a decode failure.
fn pcars_string(raw: &[u8]) -> Result<String, Short> {
    let end = raw.iter().position(|&b| b == 0).ok_or(Short)?;
    Ok(decode_latin1(&raw[..end]))
}

/// `true` when every enumerated bit-field holds a declared value
/// (`GameState`, `SessionState`, `RaceState`, `PitMode`, `PitSchedule`, `FlagReason`, `Sector`).
fn enums_valid(f: &Fields) -> Result<bool, Short> {
    let game_session = f.u8(3)?;
    let race_state = f.u8(10)?;
    let highest_flag = f.u8(98)?;
    let pit = f.u8(99)?;
    let sectors_ok =
        (0..PARTICIPANTS).try_fold(true, |ok, i| Ok::<_, Short>(ok && f.u8(464 + 16 * i + 11)? & 7 <= 5))?;
    Ok(sectors_ok
        && game_session & 7 <= 5
        && game_session >> 4 <= 6
        && race_state & 7 <= 6
        && pit & 7 <= 4
        && (pit & 0xF0) >> 4 <= 3
        && (highest_flag & 0xF0) >> 4 <= 3)
}

impl Packet {
    /// Decodes a datagram. `Err` where decoding fails (short packet, enum value
    /// outside its declaration, name without NUL).
    fn read(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        match f.u8(2)? & 3 {
            0 => {
                f.bytes(0, TELEMETRY_LEN)?;
                if !enums_valid(&f)? {
                    return Err(Short);
                }
                let gear = f.u8(128)? & 0x0F;
                let v3 = |o: usize| -> Result<[f64; 3], Short> { Ok([f.f32(o)?, f.f32(o + 4)?, f.f32(o + 8)?]) };
                Ok(Packet::Telemetry(Motion {
                    max_rpm: f64::from(f.u16(126)?),
                    rpm: f64::from(f.u16(124)?),
                    gear: if gear == 15 { -1.0 } else { f64::from(gear) },
                    speed: f.f32(120)?,
                    steering: f64::from(f.i8(8)?),
                    orientation: v3(136)?,
                    local_velocity: v3(148)?,
                    local_acceleration: v3(184)?,
                    terrain: [f.u8(224)?, f.u8(225)?, f.u8(226)?, f.u8(227)?],
                }))
            }
            1 => {
                f.bytes(0, STRINGS_LEN)?;
                let names: Result<Vec<String>, Short> =
                    (0..20).map(|i| pcars_string(f.bytes(3 + NAME_LEN * i, NAME_LEN)?)).collect();
                Ok(Packet::Strings { car_name: names?.swap_remove(0) })
            }
            2 => {
                f.bytes(0, ADDITIONAL_STRINGS_LEN)?;
                for i in 0..16 {
                    pcars_string(f.bytes(4 + NAME_LEN * i, NAME_LEN)?)?;
                }
                Ok(Packet::AdditionalStrings)
            }
            _ => Ok(Packet::Other),
        }
    }
}

/// The receive thread's handle to the socket (a second handle to the dispatcher's socket).
type ThreadSocket = Arc<Mutex<Box<dyn Datagrams>>>;

/// State the dispatcher and the receive thread share.
#[derive(Default)]
struct Shared {
    /// The connected flag.
    connected: AtomicBool,
    /// The car name (`0`, shown as no name, until a participant-strings packet names the car).
    car_name: Mutex<Option<String>>,
    /// The socket as the thread sees it.
    thread_socket: Mutex<Option<ThreadSocket>>,
    stop: AtomicBool,
}

impl Shared {
    fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    fn set_car_name(&self, name: String) {
        *self.car_name.lock().unwrap_or_else(PoisonError::into_inner) = Some(name);
    }

    fn clear_car_name(&self) {
        *self.car_name.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn car_name(&self) -> String {
        self.car_name.lock().unwrap_or_else(PoisonError::into_inner).clone().unwrap_or_default()
    }

    fn thread_socket(&self) -> Option<ThreadSocket> {
        self.thread_socket.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_thread_socket(&self, socket: Option<ThreadSocket>) {
        *self.thread_socket.lock().unwrap_or_else(PoisonError::into_inner) = socket;
    }
}

/// Pause of 1/180 s after each pass while connected.
const THREAD_CONNECTED_PAUSE: Duration = Duration::from_nanos(if cfg!(windows) { 5_555_600 } else { 5_555_556 });
/// One-second pause while not connected.
const THREAD_IDLE_PAUSE: Duration = Duration::from_secs(1);

/// The receive thread, started with the listener. While connected it competes with the receive step
/// for the datagrams of the socket (whichever thread is waiting gets each one), takes the car name from
/// participant-strings packets, forwards nothing and ignores parse errors; a receive error clears the connected flag.
/// While not connected it sets the car name to 0.
struct MainLoop {
    shared: Arc<Shared>,
    /// The last datagram the thread received (parsed again after a failed receive).
    last: Option<Vec<u8>>,
}

impl MainLoop {
    fn run(mut self) {
        while !self.shared.stop.load(Ordering::Relaxed) {
            let pause = self.pass();
            sleep_unless_stopped(&self.shared.stop, pause);
        }
    }

    /// One loop iteration up to its pause; returns the pause.
    fn pass(&mut self) -> Duration {
        if !self.shared.connected() {
            self.shared.clear_car_name();
            return THREAD_IDLE_PAUSE;
        }
        // The handle is fetched for every receive and released afterwards, so each receive
        // uses the current socket.
        let received = match self.shared.thread_socket() {
            Some(socket) => socket.lock().unwrap_or_else(PoisonError::into_inner).receive(RECEIVE_SIZE),
            None => Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "no socket")),
        };
        match received {
            Ok((data, _)) => self.last = Some(data),
            Err(e) => {
                self.shared.set_connected(false);
                tracing::debug!("pcars udp receive from data2 error {e}");
            }
        }
        match self.last.as_deref().map(Packet::read) {
            Some(Ok(Packet::Strings { car_name })) => self.shared.set_car_name(car_name),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => tracing::debug!("pcars exception while reading car and track"),
        }
        THREAD_CONNECTED_PAUSE
    }
}

/// Sleep in short slices so dropping the source ends the thread promptly.
fn sleep_unless_stopped(stop: &AtomicBool, duration: Duration) {
    let slice = Duration::from_millis(20);
    let mut left = duration;
    while !left.is_zero() && !stop.load(Ordering::Relaxed) {
        let step = left.min(slice);
        std::thread::sleep(step);
        left -= step;
    }
}

/// The Project CARS UDP listener, with its receive
/// thread.
pub struct ProjectCarsSource {
    settings: ListenerSettings,
    imported_port: Option<ImportedPort>,
    net: Box<dyn Network>,
    forwarder: Forwarder,
    /// The socket as the dispatcher uses it.
    socket: Option<Box<dyn Datagrams>>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    motion: Option<Motion>,
}

impl ProjectCarsSource {
    /// Starts the receive thread.
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork)).start_worker()
    }

    /// Cache the configured port as an integer without applying the socket range yet.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork)).start_worker()
    }

    fn start_worker(mut self) -> Self {
        let main_loop = MainLoop { shared: self.shared.clone(), last: None };
        self.thread = std::thread::Builder::new().name("threadpcars".into()).spawn(move || main_loop.run()).ok();
        self
    }

    fn with_imported_network(port: ImportedPort, net: Box<dyn Network>) -> Self {
        let mut source = Self::with_network(ListenerSettings::with_port(0), net);
        source.imported_port = Some(port);
        source
    }

    /// Without the receive thread (tests drive [`MainLoop::pass`] themselves).
    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        let forwarder = Forwarder::default();
        ProjectCarsSource {
            settings,
            imported_port: None,
            net,
            forwarder,
            socket: None,
            shared: Arc::default(),
            thread: None,
            motion: None,
        }
    }

    /// The liveness test while not connected: a new socket (the previous one is released
    /// first), bind, take one datagram and accept a telemetry or a participant-strings packet.
    fn connect(&mut self) -> bool {
        self.connect_with(|message| tracing::debug!("{message}"))
    }

    fn connect_with(&mut self, mut report: impl FnMut(String)) -> bool {
        self.socket = None;
        self.shared.set_thread_socket(None);
        let endpoint = Endpoint::any(self.settings.port, RECEIVE_TIMEOUT);
        let opened = match self.imported_port {
            Some(port) => self.net.bind_imported(&endpoint, port),
            None => self.net.bind(&endpoint),
        };
        let mut socket = match opened {
            Ok(s) => s,
            Err(e) => {
                let stage = if self.imported_port.is_some() && socket_creation_failed(&e) { "socket" } else { "bind" };
                report(format!("pcars udp {stage} error {e}"));
                self.shared.set_connected(false);
                return false;
            }
        };
        let Ok((data, _)) = socket.receive(RECEIVE_SIZE) else {
            report("pcars udp error readfrom 1".into());
            self.shared.set_connected(false);
            return false;
        };
        self.forwarder.forward(&data);
        if !matches!(Packet::read(&data), Ok(Packet::Telemetry(_) | Packet::Strings { .. })) {
            self.shared.set_connected(false);
            return false;
        }
        match socket.try_clone() {
            Ok(thread_socket) => self.shared.set_thread_socket(Some(Arc::new(Mutex::new(thread_socket)))),
            Err(e) => tracing::warn!("pcars: no second socket handle for the receive thread: {e}"),
        }
        self.socket = Some(socket);
        self.shared.set_connected(true);
        true
    }

    /// The receive step: one more datagram from the socket. A failure clears the connected flag and leaves
    /// the socket open until the next liveness test replaces it.
    fn receive(&mut self) -> bool {
        let Some(socket) = self.socket.as_mut() else { return false };
        let data = match socket.receive(RECEIVE_SIZE) {
            Ok((data, _)) => data,
            Err(e) => {
                tracing::debug!("pcars udp error receive from {e}");
                self.shared.set_connected(false);
                return false;
            }
        };
        self.forwarder.forward(&data);
        match Packet::read(&data) {
            Ok(Packet::Telemetry(m)) => {
                self.motion = Some(m);
                true
            }
            Ok(Packet::Strings { car_name }) => {
                self.shared.set_car_name(car_name);
                true
            }
            Ok(_) => true,
            Err(_) => {
                tracing::debug!("pcars udp receive from data2 error");
                self.shared.set_connected(false);
                false
            }
        }
    }

    /// Writes the channels: the copied values, 0 for everything else.
    fn write_channels(&self, t: &mut Telemetry) {
        zero_listener_channels(t);
        t.vehicle = self.shared.car_name();
        let Some(m) = self.motion else { return };
        t.max_rpm = m.max_rpm;
        t.rpm = m.rpm;
        t.gear = m.gear;
        t.speed = m.speed;
        t.steering_wheel_angle = m.steering;
        t.lateral_acceleration = m.local_acceleration[0] / 10.0;
        t.vertical_acceleration = m.local_acceleration[1] / 1000.0;
        // The four suspension channels carry the vertical acceleration.
        t.suspension_travel_lf = m.local_acceleration[1] / 1000.0;
        t.suspension_travel_lr = m.local_acceleration[1] / 1000.0;
        t.suspension_travel_rf = m.local_acceleration[1] / 1000.0;
        t.suspension_travel_rr = m.local_acceleration[1] / 1000.0;
        t.longitudinal_acceleration = -m.local_acceleration[2];
        t.vertical_velocity = m.local_velocity[1];
        t.lateral_velocity = -m.local_velocity[0];
        t.longitudinal_velocity = m.local_velocity[2];
        t.roll = rad_to_deg(m.orientation[2]);
        t.pitch = rad_to_deg(m.orientation[0]);
        t.yaw = rad_to_deg(m.orientation[1]);
        let rumble = |terrain: u8| if RUMBLE_TERRAIN.contains(&terrain) { 40.0 } else { 0.0 };
        t.rumble_pitch_lf = rumble(m.terrain[0]);
        t.rumble_pitch_rf = rumble(m.terrain[1]);
        t.rumble_pitch_lr = rumble(m.terrain[2]);
        t.rumble_pitch_rr = rumble(m.terrain[3]);
    }
}

impl Source for ProjectCarsSource {
    fn id(&self) -> &'static str {
        "projectcarsudp"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};
        if name == "car_name" {
            return Some(
                self.shared
                    .car_name
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map_or(Int(0), |name| Str(name.clone())),
            );
        }
        let Some(m) = self.motion else { return Some(Int(0)) };
        Some(match name {
            "max_rpm" => Int(m.max_rpm as i64),
            "rpm" => Int(m.rpm as i64),
            "gear" => Int(m.gear as i64),
            "steering_wheel_angle" => Int(m.steering as i64),
            "speed" => Float(m.speed),
            "lateral_acceleration" => Float(m.local_acceleration[0] / 10.0),
            "vertical_acceleration"
            | "suspension_travel_lf"
            | "suspension_travel_lr"
            | "suspension_travel_rf"
            | "suspension_travel_rr" => Float(m.local_acceleration[1] / 1000.0),
            "longitudinal_acceleration" => Float(-m.local_acceleration[2]),
            "vertical_velocity" => Float(m.local_velocity[1]),
            "lateral_velocity" => Float(-m.local_velocity[0]),
            "longitudinal_velocity" => Float(m.local_velocity[2]),
            "roll" => Float(rad_to_deg(m.orientation[2])),
            "pitch" => Float(rad_to_deg(m.orientation[0])),
            "yaw" => Float(rad_to_deg(m.orientation[1])),
            "rumble_pitch_lf" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[0]) { 40 } else { 0 }),
            "rumble_pitch_rf" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[1]) { 40 } else { 0 }),
            "rumble_pitch_lr" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[2]) { 40 } else { 0 }),
            "rumble_pitch_rr" => Int(if RUMBLE_TERRAIN.contains(&m.terrain[3]) { 40 } else { 0 }),
            _ => Int(0),
        })
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.receive())
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.shared.connected() || self.connect())
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        self.write_channels(telemetry);
        Ok(fresh)
    }
}

impl Drop for ProjectCarsSource {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_creation_failure_uses_the_socket_error_catch_without_losing_motion() {
        use crate::udp_racing::socket::socket_creation_error;
        struct Fails;
        impl Network for Fails {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn Datagrams>> {
                Err(socket_creation_error(std::io::Error::other("allocation failed")))
            }
        }
        let mut source = ProjectCarsSource::with_imported_network(ImportedPort::Valid(DEFAULT_PORT), Box::new(Fails));
        let Packet::Telemetry(motion) = Packet::read(&telemetry_packet()).unwrap() else { panic!("telemetry") };
        source.motion = Some(motion);
        let mut reports = Vec::new();
        assert!(!source.connect_with(|line| reports.push(line)));
        assert_eq!(reports, ["pcars udp socket error allocation failed"]);
        assert!(!source.shared.connected());
        assert_eq!(source.motion, Some(motion));
    }

    #[test]
    fn native_imported_port_retries_setup_then_receives_without_starting_worker() {
        use crate::udp_racing::socket::socket_creation_error;
        use std::io;
        struct Staged {
            attempts: Arc<Mutex<Vec<ImportedPort>>>,
            ready: FakeNetwork,
        }
        impl Network for Staged {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native source must bind its cached integer")
            }
            fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
                assert_eq!(endpoint.timeout, Some(RECEIVE_TIMEOUT));
                assert!(!endpoint.broadcast && !endpoint.reuse_address);
                let step = {
                    let mut attempts = self.attempts.lock().unwrap();
                    attempts.push(port);
                    attempts.len()
                };
                match step {
                    1 => Err(socket_creation_error(io::Error::other("create failed"))),
                    2 => Err(io::Error::other("bind failed")),
                    3 => Err(io::Error::other("timeout failed")),
                    _ => {
                        let mut endpoint = endpoint.clone();
                        endpoint.bind.set_port(port.for_bind()?);
                        self.ready.bind(&endpoint)
                    }
                }
            }
        }
        let ready = FakeNetwork::default();
        let attempts = Arc::new(Mutex::new(Vec::new()));
        ready.push(6001, &telemetry_packet());
        ready.push(6001, &telemetry_packet());
        let mut source = ProjectCarsSource::with_imported_network(
            ImportedPort::Valid(6001),
            Box::new(Staged { attempts: attempts.clone(), ready }),
        );
        assert!(source.thread.is_none());
        let Packet::Telemetry(motion) = Packet::read(&telemetry_packet()).unwrap() else { panic!("telemetry") };
        source.motion = Some(motion);
        let mut reports = Vec::new();
        for _ in 0..3 {
            assert!(!source.connect_with(|line| reports.push(line)));
            assert_eq!(source.motion, Some(motion));
            assert!(!source.shared.connected());
            assert!(source.shared.thread_socket().is_none());
        }
        assert_eq!(
            reports,
            [
                "pcars udp socket error create failed",
                "pcars udp bind error bind failed",
                "pcars udp bind error timeout failed"
            ]
        );
        assert!(source.connect_with(|line| reports.push(line)));
        assert!(source.shared.connected());
        assert!(source.shared.thread_socket().is_some());
        assert!(source.receive());
        assert_eq!(*attempts.lock().unwrap(), [ImportedPort::Valid(6001); 4]);
        assert!(source.thread.is_none());
    }

    #[test]
    fn native_out_of_range_bind_returns_false_and_keeps_prior_telemetry() {
        struct OutOfRange;
        impl Network for OutOfRange {
            fn bind(&mut self, _: &Endpoint) -> std::io::Result<Box<dyn Datagrams>> {
                panic!("range validation must not invent a valid port")
            }
        }
        let mut source = ProjectCarsSource::with_imported_network(ImportedPort::OutOfRange, Box::new(OutOfRange));
        let Packet::Telemetry(motion) = Packet::read(&telemetry_packet()).unwrap() else { panic!("telemetry") };
        source.motion = Some(motion);
        let mut reports = Vec::new();
        assert!(!source.connect_with(|line| reports.push(line)));
        assert_eq!(reports, ["pcars udp bind error bind(): port must be 0-65535."]);
        assert_eq!(source.motion, Some(motion));
    }
    use crate::udp_racing::support::net::fake::FakeNetwork;

    #[test]
    fn native_output_kinds() {
        let mut s =
            ProjectCarsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Int(0)));
        let Packet::Telemetry(motion) = Packet::read(&telemetry_packet()).unwrap() else { panic!("telemetry") };
        s.motion = Some(motion);
        for (name, value) in [
            ("rpm", LegacyValue::Int(6500)),
            ("max_rpm", LegacyValue::Int(9000)),
            ("gear", LegacyValue::Int(3)),
            ("steering_wheel_angle", LegacyValue::Int(-64)),
            ("speed", LegacyValue::Float(25.5)),
            ("suspension_travel_lf", LegacyValue::Float(9.81)),
            ("lateral_velocity", LegacyValue::Float(-2.0)),
            ("rumble_pitch_lf", LegacyValue::Int(40)),
            ("rumble_pitch_rr", LegacyValue::Int(0)),
            ("track_loc", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(value), "{name}");
        }
        s.shared.set_car_name(String::new());
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Str(String::new())));
        let mut main_loop = main_loop(&s);
        main_loop.pass();
        assert_eq!(s.native_channel("car_name"), Some(LegacyValue::Int(0)), "offline name assignment is integer zero");
    }

    #[test]
    fn native_adapter_reads_current_shared_name_without_the_receiver_thread() {
        use crate::derive::{Listener, SourceListener};
        use crate::NativeRead;
        let s =
            ProjectCarsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::<FakeNetwork>::default());
        let shared = s.shared.clone();
        let mut listener = SourceListener::new(s);
        listener.receive();
        shared.set_car_name("Race car".into());
        assert_eq!(listener.read_native("car_name"), NativeRead::Value(LegacyValue::Str("Race car".into())));
        shared.clear_car_name();
        assert_eq!(listener.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
    }

    /// A type-0 (telemetry) packet (header u16 + u8, then the fields).
    fn telemetry_packet() -> Vec<u8> {
        let mut p = vec![0u8; TELEMETRY_LEN];
        p[2] = 0b0000_0100; // sequence 1, type 0
        p[8] = (-64i8) as u8;
        p[120..124].copy_from_slice(&25.5f32.to_le_bytes());
        p[124..126].copy_from_slice(&6500u16.to_le_bytes());
        p[126..128].copy_from_slice(&9000u16.to_le_bytes());
        p[128] = 0x63; // 6 gears, in 3rd
        p[136..140].copy_from_slice(&0.5f32.to_le_bytes());
        p[148..152].copy_from_slice(&2.0f32.to_le_bytes());
        p[156..160].copy_from_slice(&30.0f32.to_le_bytes());
        p[184..188].copy_from_slice(&5.0f32.to_le_bytes());
        p[188..192].copy_from_slice(&9810.0f32.to_le_bytes());
        p[192..196].copy_from_slice(&(-4.0f32).to_le_bytes());
        p[224..228].copy_from_slice(&[10, 25, 40, 7]);
        p
    }

    fn strings_packet(car: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; STRINGS_LEN];
        p[2] = 1;
        p[3..3 + car.len()].copy_from_slice(car);
        p
    }

    fn connected(first: &[u8]) -> (ProjectCarsSource, FakeNetwork) {
        let net = FakeNetwork::default();
        let mut s = ProjectCarsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, first);
        assert!(s.is_running().unwrap());
        (s, net)
    }

    #[test]
    fn check_status_validates_only_the_first_datagram() {
        // Liveness: type 0 or 1 connects; afterwards it answers true without reading
        let (mut s, net) = connected(&telemetry_packet());
        assert_eq!(net.bound()[0].bind, "0.0.0.0:5606".parse().unwrap());
        assert!(s.is_running().unwrap());
        assert_eq!(net.bound().len(), 1);

        let net = FakeNetwork::default();
        let mut s = ProjectCarsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        net.push(DEFAULT_PORT, &[0, 0, 3]);
        assert!(!s.is_running().unwrap(), "type 3 is not accepted");
    }

    #[test]
    fn telemetry_mapping() {
        // Receive step, packet type 0
        let (mut s, net) = connected(&strings_packet(b"Formula Rookie"));
        net.push(DEFAULT_PORT, &telemetry_packet());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (6500.0, 9000.0, 3.0, 25.5));
        assert_eq!(t.steering_wheel_angle, -64.0);
        assert_eq!(t.lateral_acceleration, 0.5);
        assert_eq!(t.vertical_acceleration, 9.81);
        assert_eq!(t.suspension_travel_rr, 9.81);
        assert_eq!(t.longitudinal_acceleration, 4.0);
        assert_eq!(t.lateral_velocity, -2.0);
        assert_eq!(t.longitudinal_velocity, 30.0);
        assert!((t.pitch - 28.64788975654116).abs() < 1e-9);
        assert_eq!(
            [t.rumble_pitch_lf, t.rumble_pitch_rf, t.rumble_pitch_lr, t.rumble_pitch_rr],
            [40.0, 40.0, 40.0, 0.0]
        );
        assert_eq!(t.vehicle, "", "the strings packet was consumed by the liveness test");
    }

    #[test]
    fn reverse_gear_and_car_name() {
        // Receive step: gear 15 -> -1; packet type 1 -> car_name
        let (mut s, net) = connected(&telemetry_packet());
        let mut p = telemetry_packet();
        p[128] = 0x6F;
        net.push(DEFAULT_PORT, &strings_packet(b"Formula Rookie"));
        net.push(DEFAULT_PORT, &p);
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.vehicle, "Formula Rookie");
        assert_eq!(t.rpm, 0.0, "no telemetry packet yet: every channel reads 0");
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.gear, -1.0);
    }

    #[test]
    fn malformed_packets_disconnect() {
        // Decoding: an invalid sector, a missing NUL or a short buffer fails -> not connected
        let mut bad_sector = telemetry_packet();
        bad_sector[464 + 11] = 6;
        let mut no_nul = strings_packet(b"x");
        no_nul[3..67].fill(b'a');
        for bad in [bad_sector, no_nul, vec![0u8, 0, 0, 1]] {
            let (mut s, net) = connected(&telemetry_packet());
            net.push(DEFAULT_PORT, &bad);
            let mut t = Telemetry::off();
            assert!(!s.poll(&mut t).unwrap());
            net.push(DEFAULT_PORT, &telemetry_packet());
            assert!(s.is_running().unwrap(), "reconnects");
            assert_eq!(net.bound().len(), 2);
        }
    }

    #[test]
    fn receive_timeout_disconnects() {
        // Receive step: a receive error -> not connected
        let (mut s, net) = connected(&telemetry_packet());
        let mut t = Telemetry::off();
        assert!(!s.poll(&mut t).unwrap());
        assert!(!s.is_running().unwrap());
        assert_eq!(net.bound().len(), 2);
    }

    fn main_loop(s: &ProjectCarsSource) -> MainLoop {
        MainLoop { shared: s.shared.clone(), last: None }
    }

    #[test]
    fn main_loop_takes_the_car_name_from_the_shared_socket() {
        // Receive thread: while connected, a 2400-byte receive on the socket, packet type 1 ->
        // the car name; a 1/180 s pause. Nothing is forwarded.
        let sink = crate::udp_racing::support::net::fake::ForwardSink::new();
        let net = FakeNetwork::default();
        let mut s = ProjectCarsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        let (forwarder, parameters) = crate::udp_racing::forward::test_forwarder("projectcarsudp", &[sink.target()]);
        parameters.set_listener_running("none");
        s.forwarder = forwarder;
        net.push(DEFAULT_PORT, &telemetry_packet());
        assert!(s.is_running().unwrap());
        parameters.set_listener_running("projectcarsudp");
        let mut t = Telemetry::off();
        net.push(DEFAULT_PORT, &strings_packet(b"Formula Rookie"));
        let mut thread = main_loop(&s);
        assert_eq!(thread.pass(), THREAD_CONNECTED_PAUSE);
        net.push(DEFAULT_PORT, &telemetry_packet());
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.vehicle, "Formula Rookie");
        assert_eq!(sink.received().len(), 1, "only receiveData's datagram is forwarded");
    }

    #[test]
    fn main_loop_receive_error_disconnects_and_the_next_pass_clears_the_name() {
        // Receive thread: a receive error -> not connected; the previous datagram is parsed again;
        // while not connected the car name is 0 and the pause is one second.
        let (s, net) = connected(&telemetry_packet());
        let mut thread = main_loop(&s);
        net.push(DEFAULT_PORT, &strings_packet(b"GT3"));
        thread.pass();
        assert_eq!(s.shared.car_name(), "GT3");
        assert_eq!(thread.pass(), THREAD_CONNECTED_PAUSE, "timeout");
        assert_eq!(s.shared.car_name(), "GT3", "the previous data2 is parsed again");
        assert!(!s.shared.connected());
        assert_eq!(thread.pass(), THREAD_IDLE_PAUSE);
        let mut t = Telemetry::off();
        s.write_channels(&mut t);
        assert_eq!(t.vehicle, "");
    }

    #[test]
    fn main_loop_waits_while_not_connected() {
        // Receive thread, not connected: car_name = 0, a one-second pause; no socket is touched.
        let net = FakeNetwork::default();
        let s = ProjectCarsSource::with_network(ListenerSettings::with_port(DEFAULT_PORT), Box::new(net.clone()));
        s.shared.set_car_name("old".into());
        assert_eq!(main_loop(&s).pass(), THREAD_IDLE_PAUSE);
        assert_eq!(s.shared.car_name(), "");
        assert!(net.bound().is_empty());
    }
}

#[cfg(test)]
mod pause_tests {
    /// A pause of 1/180 s (binary64 0x3f76c16c16c16c17) after each connected pass.
    #[test]
    fn connected_pause_is_the_original_sleep() {
        assert_eq!(Ok(super::THREAD_CONNECTED_PAUSE), openrigcue_model::python_time::sleep_duration(1.0 / 180.0));
    }
}

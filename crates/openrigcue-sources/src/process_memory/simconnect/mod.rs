//! Flight Simulator X, Prepar3D and Microsoft Flight Simulator 2020/2024 through the SimConnect wire protocol
//! over TCP (listener keys `fsx`, `p3d`, `fs2k`). The simulator's `SimConnect.xml` enables a local IPv4 server
//! on the configured port; the listener is a direct client of that server (no `SimConnect.dll`).

pub mod aircraft;
pub mod connection;
mod native;
pub mod wire;

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use self::aircraft::{AircraftState, DataDefinition, Simulator};
use self::connection::Connection;
use self::wire::{ClientMessage, ServerMessage, FLAG_CHANGED, OBJECT_ID_USER, PERIOD_SIM_FRAME, UNUSED_DATUM_ID};
use super::listener_support::{is_listener_channel, registry_default, set_channel};
use super::python::is_space;
use crate::udp_flight::python::fold_roll;
use crate::udp_flight::{AngularRates, FlightSource};
use crate::udp_racing::socket::ImportedPort;
use crate::{NativeRead, Source, SourceError};
use socket2::{Domain, Protocol, Socket, Type};

/// `NETWORK_PORT/fsx` and `NETWORK_PORT/p3d` default (also written into `SimConnect.xml`).
pub const DEFAULT_PORT: u16 = 33445;
/// Pause between two pumps while connected (1/180 s), and the idle pause.
// The pause is binary64 0x3f76c16c16c16c17 seconds,
// rounded up rather than truncated to 5,555,555 ns.
const PUMP_PAUSE: Duration = Duration::from_nanos(if cfg!(windows) { 5_555_600 } else { 5_555_556 });
const IDLE_PAUSE: Duration = Duration::from_secs(1);
/// Definition and request ids of the listeners.
const DEFINITION_ID: u32 = 1;
const REQUEST_ID: u32 = 1;
/// Length of the pause-detection window.
const FREEZE_WINDOW: usize = 1000;
/// MSFS camera states above this are menus and the world map.
const LAST_FLIGHT_CAMERA_STATE: f64 = 6.0;

/// `CUSTOM_FEATURES/p3d_invert_yaw|roll|pitch` (`'yes'` → true), read by all three listeners.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttitudeInversion {
    pub yaw: bool,
    pub roll: bool,
    pub pitch: bool,
}

/// Settings read from the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlightSimSettings {
    pub simulator: Simulator,
    /// `NETWORK_PORT/fsx` (FSX, MSFS) or `NETWORK_PORT/p3d` (Prepar3D).
    pub port: u16,
    pub inversion: AttitudeInversion,
    /// The configured motion platform's manufacturer is Yaw VR (MSFS lets its yaw through in menus).
    pub yaw_vr: bool,
}

/// Current MOTIONBOX.model value, consulted by the MSFS menu-camera getter.
pub type NativeModelReader = Arc<dyn Fn() -> Option<LegacyValue> + Send + Sync>;

impl FlightSimSettings {
    pub fn new(simulator: Simulator) -> Self {
        FlightSimSettings { simulator, port: DEFAULT_PORT, inversion: AttitudeInversion::default(), yaw_vr: false }
    }
}

/// Open-message parameters and the header protocol of each listener.
fn identity(simulator: Simulator) -> (ClientMessage, u32) {
    match simulator {
        Simulator::Prepar3d => {
            (ClientMessage::Open { app_name: "SRSLogger", sim_id: "D3P", version: [4, 3], build: [0, 0] }, 29)
        }
        _ => (ClientMessage::Open { app_name: "SRS", sim_id: "XSF", version: [10, 0], build: [62615, 0] }, 4),
    }
}

/// The session set-up messages, in sending order.
fn setup_messages(simulator: Simulator, definitions: &[DataDefinition]) -> Vec<ClientMessage> {
    let mut messages = vec![
        identity(simulator).0,
        ClientMessage::RequestSystemState { request_id: 0, state_name: "Sim" },
        ClientMessage::SubscribeToSystemEvent { client_event_id: 3, event_name: "SimStart" },
    ];
    messages.extend(definitions.iter().map(|d| ClientMessage::AddToDataDefinition {
        definition_id: DEFINITION_ID,
        datum_name: d.name,
        units_name: d.units,
        data_type: d.data_type,
        epsilon: 0.0,
        datum_id: UNUSED_DATUM_ID,
    }));
    messages.extend([
        ClientMessage::RequestDataOnSimObject {
            request_id: REQUEST_ID,
            definition_id: DEFINITION_ID,
            object_id: OBJECT_ID_USER,
            period: PERIOD_SIM_FRAME,
            flags: FLAG_CHANGED,
            origin: 0,
            interval: 0,
            limit: 0,
        },
        ClientMessage::MapClientEventToSimEvent { event_id: 12, event_name: "FSF.STOP_EFFECTS" },
        ClientMessage::AddClientEventToNotificationGroup { group_id: 5, event_id: 12, maskable: 0 },
        ClientMessage::SetNotificationGroupPriority { group_id: 5, priority: 1 },
    ]);
    messages
}

/// The line the FSX listener prints after queueing its set-up messages: the FlyInside client-data registration
/// is not available, so nothing of that registration is sent.
const FLYINSIDE_REGISTRATION_FAILURE: &str = "Could not register FlyInside client data: module \
    'backend.listeners.simconnect.message' has no attribute 'CMapClientDataNameToID'";

/// State shared by the listener and its receive thread (the connected flag, the connection, the decoded output, the message counter).
struct Link<S> {
    connection: Option<Connection<S>>,
    connected: bool,
    aircraft: Option<AircraftState>,
    counter: u32,
}

impl<S: Read + Write> Link<S> {
    /// One iteration of the receive loop while connected: pump once and handle the message. Any
    /// failure drops the connection flag (the next liveness check reconnects).
    fn receive_decoded(&mut self, definitions: &[DataDefinition]) -> Option<AircraftState> {
        let connection = self.connection.as_mut()?;
        match connection.pump() {
            Ok(Some(ServerMessage::SimObjectData { data, .. })) => match AircraftState::decode(definitions, &data) {
                Ok(state) => return Some(state),
                Err(error) => self.drop_connection(&error),
            },
            // There is no handler for this message class.
            Ok(Some(ServerMessage::Unhandled { repr })) => {
                crate::printed::line(format!("ERROR: no handler for {repr}"));
            }
            // Open and system-state messages need no action; an event matches no client event entry.
            Ok(Some(_)) | Ok(None) => {}
            Err(connection::PumpError::Io(error)) => self.drop_connection(&crate::printed::os_error(&error)),
            Err(connection::PumpError::Closed) => {
                // A closed connection: the printed text names the connection object and its address.
                let closed = format!("<backend.listeners.simconnect.connection.Connection object at {connection:p}>");
                self.drop_connection(&closed);
            }
            Err(connection::PumpError::Wire(error)) => self.drop_connection(&error),
        }
        None
    }

    #[cfg(test)]
    fn receive_once(&mut self, definitions: &[DataDefinition]) {
        if let Some(decoded) = self.receive_decoded(definitions) {
            for def in definitions {
                self.aircraft.get_or_insert_with(AircraftState::empty).publish(def.datum, &decoded);
            }
        }
    }

    fn drop_connection(&mut self, error: &dyn std::fmt::Display) {
        // Any failure is printed after "Exception 2".
        crate::printed::line(format!("Exception 2 {error}"));
        self.connected = false;
    }
}

/// The pause gate of the FSX and Prepar3D listeners: every request appends the attitude and
/// acceleration sum to a 1000-entry FIFO, and while that FIFO holds one value only (or a single entry) the
/// request reads 0, which is how a paused simulator is detected.
#[derive(Debug, Clone, Default, PartialEq)]
struct FreezeDetector {
    last: Option<f64>,
    /// Trailing entries equal to `last` (NaN equals nothing).
    run: usize,
    len: usize,
}

impl FreezeDetector {
    /// Appends `sum` to the window, then tests whether the window holds one value only.
    fn observe(&mut self, sum: f64) -> bool {
        self.len = (self.len + 1).min(FREEZE_WINDOW);
        self.run = if self.last == Some(sum) { (self.run + 1).min(FREEZE_WINDOW) } else { 1 };
        self.last = Some(sum);
        self.len <= 1 || (self.run >= self.len && !sum.is_nan())
    }
}

/// What one channel read answered.
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Integer(i64),
    Number(f64),
    Text(String),
    /// An internal lookup failed; the read answers integer zero.
    Raise,
}

impl Answer {
    fn number(&self) -> f64 {
        match self {
            Self::Integer(n) => *n as f64,
            Self::Number(n) => *n,
            _ => 0.0,
        }
    }

    fn native(self) -> NativeRead {
        match self {
            Self::Integer(n) => NativeRead::Value(LegacyValue::Int(n)),
            Self::Number(n) => NativeRead::Value(LegacyValue::Float(n)),
            Self::Text(text) => NativeRead::Value(LegacyValue::Str(text)),
            Self::Raise => NativeRead::Raised,
        }
    }

    /// The first operand survives equal and unordered comparisons, including its numeric kind.
    fn maximum(values: [Self; 4]) -> Self {
        values.into_iter().reduce(|best, value| if value.number() > best.number() { value } else { best }).unwrap()
    }
}

/// One channel read of the dispatcher's processed tick, whether the tick keeps its answer, and
/// what the dispatcher writes into the channel when the read fails (`None`: unchanged).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Call {
    request: &'static str,
    kept: bool,
    /// The dispatcher stores a numeric answer in the registry channel of that name.
    stores_answer: bool,
    on_raise: Option<f64>,
}

/// Registry channels without `auto_load`: the dispatcher's registry loop does not ask for them.
const NOT_AUTO_LOADED: &[&str] = &[
    "pitch",
    "roll",
    "yaw",
    "lateral_velocity",
    "speed",
    "speed_kmh",
    "front_tl",
    "max_speed",
    "max_speed_kmh",
    "steering_wheel_max_angle",
    "yaw_angle",
    "cannon_firing",
    "missile_firing",
];

/// Numeric compatibility replay of a processed flying-game tick, in order
/// (with DAMS mode 0 and angle mode): names, speed (twice), the two velocities, lateral velocity again, the
/// top-speed speed read, the registry loop, then yaw rate, yaw, pitch and roll. Every call counts in the FIFO.
/// Typed dispatch performs its actual conditional reads directly. In this replay, the kept lateral velocity
/// read and the yaw / pitch / roll reads store 0 when they fail,
/// a registry call stores its registry default.
fn dispatcher_calls() -> Vec<Call> {
    let call = |request, kept, on_raise| Call { request, kept, stores_answer: is_listener_channel(request), on_raise };
    let mut calls = vec![
        call("car_name", true, None),
        call("track_loc", true, None),
        call("speed", false, None),
        call("speed", true, None),
        call("lateral_velocity", false, None),
        call("longitudinal_velocity", false, None),
        call("lateral_velocity", true, Some(0.0)),
        call("speed", false, None),
    ];
    calls.extend(
        Telemetry::CHANNELS
            .iter()
            .filter(|name| !NOT_AUTO_LOADED.contains(name))
            .map(|name| call(name, true, registry_default(name))),
    );
    calls.extend([
        call("yaw_rate", true, None),
        call("yaw", true, Some(0.0)),
        call("pitch", true, Some(0.0)),
        call("roll", true, Some(0.0)),
    ]);
    calls
}

/// The FSX, Prepar3D and MSFS listeners and their
/// receive thread, which runs from construction until the source is dropped.
pub struct FlightSimulator<S: Read + Write + Send + 'static = TcpStream> {
    settings: FlightSimSettings,
    model_reader: Option<NativeModelReader>,
    definitions: Arc<Vec<DataDefinition>>,
    link: Arc<Mutex<Link<S>>>,
    connect: Box<dyn FnMut(u16) -> io::Result<S> + Send>,
    freeze: Mutex<FreezeDetector>,
    previous_wind: Mutex<[f64; 3]>,
    /// The `max_rpm1`..`max_rpm4` entries, created by the max_rpm getter and retained across received blocks.
    estimated_rpm: Mutex<[Option<Answer>; 4]>,
    rates: AngularRates,
    calls: Vec<Call>,
    stop: Arc<AtomicBool>,
    receiver: Option<JoinHandle<()>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A blocking TCP connect to `127.0.0.1:<port>`, then non-blocking traffic.
fn connect_tcp(port: u16) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_nonblocking(true)?;
    Ok(stream)
}

/// Shared ownership of the listener's socket and its connection; cloning this value does
/// not duplicate an OS socket handle.
pub struct NativeSimConnectStream(Arc<Socket>);

impl Read for NativeSimConnectStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(bytes)
    }
}

impl Write for NativeSimConnectStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&*self.0).write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

trait ConnectSocket {
    fn connect_port(&self, port: u16) -> io::Result<()>;
    fn nonblocking(&self) -> io::Result<()>;
}

impl ConnectSocket for Socket {
    fn connect_port(&self, port: u16) -> io::Result<()> {
        self.connect(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into())
    }
    fn nonblocking(&self) -> io::Result<()> {
        self.set_nonblocking(true)
    }
}

fn connect_native<S: ConnectSocket>(
    slot: &mut Option<Arc<S>>,
    port: ImportedPort,
    create: impl FnOnce() -> io::Result<S>,
) -> io::Result<Arc<S>> {
    let socket = Arc::new(create()?);
    // The shared socket is assigned before connect; a creation failure leaves the old object.
    *slot = Some(socket.clone());
    let port = match port {
        ImportedPort::Valid(port) => port,
        ImportedPort::OutOfRange => return Err(io::ErrorKind::InvalidInput.into()),
    };
    socket.connect_port(port)?;
    // This is set before the previous connection object is replaced.
    socket.nonblocking()?;
    Ok(socket)
}

impl FlightSimulator<NativeSimConnectStream> {
    /// The listener's port is converted to an integer once at construction, with range checking deferred to connect.
    pub fn from_imported_port(settings: FlightSimSettings, port: ImportedPort) -> Self {
        let mut socket = None;
        Self::with_connector(
            settings,
            Box::new(move |_| {
                connect_native(&mut socket, port, || Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)))
                    .map(NativeSimConnectStream)
            }),
        )
    }
}

impl FlightSimulator<TcpStream> {
    pub fn new(settings: FlightSimSettings) -> Self {
        Self::with_connector(settings, Box::new(connect_tcp))
    }
}

impl<S: Read + Write + Send + 'static> FlightSimulator<S> {
    /// A listener with its receive thread, connecting through `connect`.
    pub fn with_connector(settings: FlightSimSettings, connect: Box<dyn FnMut(u16) -> io::Result<S> + Send>) -> Self {
        let mut source = Self::without_receiver(settings, connect);
        source.receiver = source.spawn_receiver();
        source
    }

    fn without_receiver(settings: FlightSimSettings, connect: Box<dyn FnMut(u16) -> io::Result<S> + Send>) -> Self {
        FlightSimulator {
            settings,
            model_reader: None,
            definitions: Arc::new(settings.simulator.definitions()),
            link: Arc::new(Mutex::new(Link { connection: None, connected: false, aircraft: None, counter: 0 })),
            connect,
            freeze: Mutex::default(),
            previous_wind: Mutex::new([0.0; 3]),
            estimated_rpm: Mutex::default(),
            rates: AngularRates::default(),
            calls: dispatcher_calls(),
            stop: Arc::new(AtomicBool::new(false)),
            receiver: None,
        }
    }

    pub fn set_model_reader(&mut self, reader: NativeModelReader) {
        self.model_reader = Some(reader);
    }

    /// The receive thread: pump every 1/180 s while connected, idle 1 s otherwise.
    fn spawn_receiver(&self) -> Option<JoinHandle<()>> {
        let (link, definitions, stop) = (Arc::clone(&self.link), Arc::clone(&self.definitions), Arc::clone(&self.stop));
        thread::Builder::new()
            .name("fsx_main_loop".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if lock(&link).connected {
                        thread::sleep(PUMP_PAUSE);
                        let decoded = { lock(&link).receive_decoded(&definitions) };
                        if let Some(decoded) = decoded {
                            // A data block is published atomically: every output entry
                            // of one data block is assigned before a getter can run again.
                            let mut link = lock(&link);
                            let aircraft = link.aircraft.get_or_insert_with(AircraftState::empty);
                            for definition in definitions.iter() {
                                aircraft.publish(definition.datum, &decoded);
                            }
                        }
                    } else {
                        thread::sleep(IDLE_PAUSE);
                    }
                }
            })
            .ok()
    }

    /// The sign factors: -1 for an inverted axis.
    fn factor(inverted: bool) -> f64 {
        if inverted {
            -1.0
        } else {
            1.0
        }
    }

    /// The vehicle name: FSX and MSFS decode the UTF-8 title, strip whitespace (NUL padding stays), drop "Asobo"
    /// and a trailing "paint…" word; P3D keeps the Latin-1 characters before the first NUL, minus trailing
    /// spaces.
    fn vehicle_name(&self, title: &[u8]) -> Answer {
        if self.settings.simulator == Simulator::Prepar3d {
            let end = title.iter().position(|&b| b == 0).unwrap_or(title.len());
            let name: String = title[..end].iter().map(|&b| char::from(b)).collect();
            return Answer::Text(name.trim_end_matches(' ').to_owned());
        }
        let Ok(decoded) = std::str::from_utf8(title) else { return Answer::Integer(0) };
        let name = decoded.trim_matches(is_space).replace("Asobo", "");
        let last_word = name.split(' ').next_back().unwrap_or_default().to_lowercase();
        if last_word.chars().take(5).eq("paint".chars()) {
            let trimmed = name.trim_end_matches(' ');
            return Answer::Text(trimmed.rsplit_once(' ').map_or(trimmed, |(head, _)| head).to_owned());
        }
        Answer::Text(name)
    }

    /// The listener's answer to `request` when no pause gate applies.
    fn answer(&self, request: &str, a: &AircraftState) -> Answer {
        let inversion = self.settings.inversion;
        let (yaw_factor, roll_factor, pitch_factor) =
            (Self::factor(inversion.yaw), Self::factor(inversion.roll), Self::factor(inversion.pitch));
        let touchdown = |animation: f64| {
            if animation > 50.0 && a.on_ground == 1.0 {
                Answer::Number(a.vertical_acceleration)
            } else {
                Answer::Integer(0)
            }
        };
        let number = match request {
            "roll" => fold_roll(-a.roll * roll_factor),
            "pitch" => fold_roll(-a.pitch * pitch_factor),
            "yaw" => a.yaw * yaw_factor,
            "pitch_rate" => -a.pitch_rate * pitch_factor,
            "roll_rate" => -a.roll_rate * roll_factor,
            "yaw_rate" => a.yaw_rate * yaw_factor,
            "turbulence" => return self.turbulence(a.wind),
            "touchdown_nose" => return touchdown(a.gear_animation[0]),
            "touchdown_left" => return touchdown(a.gear_animation[1]),
            "touchdown_right" => return touchdown(a.gear_animation[2]),
            "rpm" => return Answer::maximum(a.engine_rpm.map(Answer::Number)),
            "max_rpm" => {
                let estimates = std::array::from_fn(|i| {
                    let percent = a.engine_max_rpm_percent[i];
                    if percent == 0.0 {
                        Answer::Integer(0)
                    } else {
                        Answer::Number(a.engine_rpm[i] * 10.0 / percent)
                    }
                });
                *lock(&self.estimated_rpm) = estimates.clone().map(Some);
                return Answer::maximum(estimates);
            }
            "lateral_acceleration" => a.lateral_acceleration / 10.0,
            "vertical_acceleration" => a.vertical_acceleration / 3.0,
            "longitudinal_acceleration" => a.longitudinal_acceleration / 3.0,
            "canopy" => a.canopy,
            // Mapped to `TURB ENG AFTERBURNER:0`, which is never requested.
            "afterburner" => return Answer::Integer(0),
            "landing_gear_nose" => a.landing_gear[0],
            "landing_gear_left" => a.landing_gear[1],
            "landing_gear_right" => a.landing_gear[2],
            "car_name" => return self.vehicle_name(&a.title),
            // Indicated airspeed in knots, passed through like any other received value.
            "speed" => a.airspeed,
            _ => return self.raw_answer(request, a),
        };
        Answer::Number(number)
    }

    /// The final branch looks the name up in the received data map, so it also exposes stored keys outside the
    /// channel registry.
    fn raw_answer(&self, request: &str, a: &AircraftState) -> Answer {
        let number = match request {
            "engine_rpm1" => a.engine_rpm[0],
            "engine_rpm2" => a.engine_rpm[1],
            "engine_rpm3" => a.engine_rpm[2],
            "engine_rpm4" => a.engine_rpm[3],
            "engine_max_perc_rpm1" => a.engine_max_rpm_percent[0],
            "engine_max_perc_rpm2" => a.engine_max_rpm_percent[1],
            "engine_max_perc_rpm3" => a.engine_max_rpm_percent[2],
            "engine_max_perc_rpm4" => a.engine_max_rpm_percent[3],
            "lateral_wind" => a.wind[0],
            "vertical_wind" => a.wind[1],
            "longitudinal_wind" => a.wind[2],
            "afterburner_1" => a.afterburner_1,
            "landing_gear_center" => a.landing_gear[0],
            "gear_animation_center" => a.gear_animation[0],
            "gear_animation_left" => a.gear_animation[1],
            "gear_animation_right" => a.gear_animation[2],
            "gear_animation_aux" => a.gear_animation_aux,
            "is_onground" => a.on_ground,
            "camera_state" if self.settings.simulator == Simulator::Msfs => a.camera_state,
            "max_rpm1" | "max_rpm2" | "max_rpm3" | "max_rpm4" => {
                let index = usize::from(request.as_bytes()[7] - b'1');
                return lock(&self.estimated_rpm)[index].clone().unwrap_or(Answer::Raise);
            }
            _ => return Answer::Raise,
        };
        Answer::Number(number)
    }

    /// `turbulence`: magnitude of the wind change since the previous answer, reported above 1 knot.
    fn turbulence(&self, wind: [f64; 3]) -> Answer {
        let mut previous = lock(&self.previous_wind);
        let delta: [f64; 3] = std::array::from_fn(|i| wind[i] - previous[i]);
        *previous = wind;
        let magnitude = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        if magnitude > 1.0 {
            Answer::Number(magnitude)
        } else {
            Answer::Integer(0)
        }
    }

    /// The listener's gate for one call (each call of FSX and P3D feeds the FIFO once). Before the first data
    /// block every call reads 0 without touching the FIFO.
    fn gate(&self, aircraft: Option<&AircraftState>) -> Gate {
        let Some(a) = aircraft else { return Gate::Closed };
        match self.settings.simulator {
            Simulator::Fsx | Simulator::Prepar3d => {
                let sum = a.roll
                    + a.pitch
                    + a.yaw
                    + a.vertical_acceleration
                    + a.longitudinal_acceleration
                    + a.lateral_acceleration;
                if lock(&self.freeze).observe(sum) {
                    Gate::Closed
                } else {
                    Gate::Open
                }
            }
            // A NaN or infinite camera state has no integer value.
            Simulator::Msfs if !a.camera_state.is_finite() => Gate::Closed,
            Simulator::Msfs if a.camera_state.trunc() > LAST_FLIGHT_CAMERA_STATE => Gate::MenusOnly,
            Simulator::Msfs => Gate::Open,
        }
    }

    /// The answer of one call under `gate`.
    fn gated_answer(&self, gate: Gate, request: &str, aircraft: Option<&AircraftState>) -> Answer {
        let yaw_vr = if gate == Gate::MenusOnly {
            match &self.model_reader {
                Some(reader) => {
                    reader()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .and_then(|name| openrigcue_model::motion_models::motion_model(&name))
                        .and_then(|model| model.fields.get("manufacturer"))
                        .and_then(|value| value.as_str())
                        == Some("Yaw VR")
                }
                None => self.settings.yaw_vr,
            }
        } else {
            false
        };
        let yaw_factor = Self::factor(self.settings.inversion.yaw);
        match (gate, aircraft) {
            // A missing output entry is handled here, like the earlier lookup
            // failures. The dispatcher sees 0.
            (Gate::Open, Some(a)) => match self.answer(request, a) {
                Answer::Raise => Answer::Integer(0),
                answer => answer,
            },
            // MSFS outside the flight views: only a Yaw VR platform keeps its yaw.
            (Gate::MenusOnly, Some(a)) if yaw_vr && request == "yaw" => Answer::Number(a.yaw * yaw_factor),
            (Gate::MenusOnly, Some(a)) if yaw_vr && request == "yaw_rate" => Answer::Number(a.yaw_rate * yaw_factor),
            _ => Answer::Integer(0),
        }
    }

    /// Runs the dispatcher's calls of one tick against the current aircraft state and keeps their answers. The
    /// pitch and roll calls also give the rate answers (the rate mode asks for `pitch_rate` / `roll_rate` there).
    fn evaluate_tick(&mut self, aircraft: Option<&AircraftState>, t: &mut Telemetry) {
        // The rate names always answer a number.
        let as_number = |answer: Answer| answer.number();
        for index in 0..self.calls.len() {
            let Call { request, kept, stores_answer, on_raise } = self.calls[index];
            let gate = self.gate(aircraft);
            let answer = self.gated_answer(gate, request, aircraft);
            match request {
                "pitch" => self.rates.pitch = as_number(self.gated_answer(gate, "pitch_rate", aircraft)),
                "roll" => self.rates.roll = as_number(self.gated_answer(gate, "roll_rate", aircraft)),
                _ => {}
            }
            if !kept {
                continue;
            }
            match (request, answer) {
                ("car_name", Answer::Text(name)) => t.vehicle = name,
                ("car_name", Answer::Number(_) | Answer::Integer(_)) => t.vehicle.clear(),
                ("track_loc", _) => t.location.clear(),
                ("yaw_rate", answer) => self.rates.yaw = as_number(answer),
                (channel, Answer::Number(n)) if stores_answer => set_channel(t, channel, n),
                (channel, Answer::Integer(n)) if stores_answer => set_channel(t, channel, n as f64),
                (channel, Answer::Raise) => {
                    if let Some(value) = on_raise {
                        set_channel(t, channel, value);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The gate decision of one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    Closed,
    /// MSFS camera state above 6.
    MenusOnly,
}

impl<S: Read + Write + Send + 'static> Source for FlightSimulator<S> {
    fn id(&self) -> &'static str {
        match self.settings.simulator {
            Simulator::Fsx => "fsx",
            Simulator::Prepar3d => "p3d",
            Simulator::Msfs => "fs2k",
        }
    }

    /// Each channel read observes the current received state and advances only its own getter state. The
    /// call reads one view of the received data; it takes a new one only after its configuration lookup, the
    /// one place where the receive thread can publish inside a getter.
    fn read_native(&self, name: &str) -> NativeRead {
        let view = std::cell::RefCell::new(lock(&self.link).aircraft.clone());
        self.native_answer_at(
            name,
            |datum| view.borrow().as_ref()?.number(datum),
            || {
                let view = view.borrow();
                let aircraft = view.as_ref()?;
                aircraft.has(aircraft::Datum::Title).then(|| aircraft.title.clone())
            },
            || *view.borrow_mut() = lock(&self.link).aircraft.clone(),
        )
        .native()
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    fn receive_native(&mut self, _t: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(lock(&self.link).connected)
    }

    /// The liveness test: connected stays connected; otherwise connect and queue the set-up messages.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        if lock(&self.link).connected {
            return Ok(true);
        }
        let stream = match (self.connect)(self.settings.port) {
            Ok(stream) => stream,
            Err(_) => return Ok(false),
        };
        let mut connection = Connection::new(stream);
        let protocol = identity(self.settings.simulator).1;
        let mut link = lock(&self.link);
        for message in setup_messages(self.settings.simulator, &self.definitions) {
            connection.queue(&message, protocol, link.counter);
            link.counter = link.counter.wrapping_add(1);
        }
        if self.settings.simulator == Simulator::Fsx {
            crate::printed::line(FLYINSIDE_REGISTRATION_FAILURE);
        }
        link.connection = Some(connection);
        link.connected = true;
        Ok(true)
    }

    /// Numeric compatibility frame. Typed dispatch uses receive_native and individual read_native calls.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let (aircraft, connected) = {
            let link = lock(&self.link);
            (link.aircraft.clone(), link.connected)
        };
        self.evaluate_tick(aircraft.as_ref(), t);
        Ok(connected)
    }
}

impl<S: Read + Write + Send + 'static> FlightSource for FlightSimulator<S> {
    fn angular_rates(&self) -> AngularRates {
        self.rates
    }
}

impl<S: Read + Write + Send + 'static> Drop for FlightSimulator<S> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(receiver) = self.receiver.take() {
            let _ = receiver.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::connection::testing::FakeStream;
    use super::wire::tests::server_frame;
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn receive_pump_uses_original_float_timeout() {
        let expected = if cfg!(windows) { 5_555_600 } else { 5_555_556 };
        assert_eq!(PUMP_PAUSE, Duration::from_nanos(expected));
    }

    #[derive(Clone, Copy)]
    enum ConnectStep {
        Create,
        Connect,
        Nonblocking,
        Success,
    }

    struct ConnectScript {
        id: usize,
        step: ConnectStep,
        trace: Arc<Mutex<Vec<String>>>,
    }

    impl ConnectSocket for ConnectScript {
        fn connect_port(&self, port: u16) -> io::Result<()> {
            self.trace.lock().unwrap().push(format!("connect{}:{port}", self.id));
            if matches!(self.step, ConnectStep::Connect) {
                Err(io::ErrorKind::ConnectionRefused.into())
            } else {
                Ok(())
            }
        }
        fn nonblocking(&self) -> io::Result<()> {
            self.trace.lock().unwrap().push(format!("nonblocking{}", self.id));
            if matches!(self.step, ConnectStep::Nonblocking) {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        }
    }

    struct FakeNativeStream(Arc<ConnectScript>);

    impl Read for FakeNativeStream {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.0.trace.lock().unwrap().push(format!("read{}", self.0.id));
            Err(io::ErrorKind::WouldBlock.into())
        }
    }

    impl Write for FakeNativeStream {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.trace.lock().unwrap().push(format!("write{}", self.0.id));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for ConnectScript {
        fn drop(&mut self) {
            self.trace.lock().unwrap().push(format!("drop{}", self.id));
        }
    }

    fn fake_connect(
        slot: &mut Option<Arc<ConnectScript>>,
        port: ImportedPort,
        id: usize,
        step: ConnectStep,
        trace: &Arc<Mutex<Vec<String>>>,
    ) -> io::Result<Arc<ConnectScript>> {
        connect_native(slot, port, || {
            trace.lock().unwrap().push(format!("create{id}"));
            if matches!(step, ConnectStep::Create) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            Ok(ConnectScript { id, step, trace: trace.clone() })
        })
    }

    #[test]
    fn native_tcp_connector_retains_each_global_and_connection_prefix() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut slot = None;
        let mut connection =
            Some(fake_connect(&mut slot, ImportedPort::Valid(33445), 0, ConnectStep::Success, &trace).unwrap());
        assert_eq!(*trace.lock().unwrap(), ["create0", "connect0:33445", "nonblocking0"]);
        assert!(fake_connect(&mut slot, ImportedPort::Valid(33445), 1, ConnectStep::Create, &trace).is_err());
        assert_eq!(slot.as_ref().unwrap().id, 0);
        assert!(fake_connect(&mut slot, ImportedPort::OutOfRange, 2, ConnectStep::Success, &trace).is_err());
        assert_eq!(slot.as_ref().unwrap().id, 2);
        assert_eq!(connection.as_ref().unwrap().id, 0);
        assert!(fake_connect(&mut slot, ImportedPort::Valid(33445), 3, ConnectStep::Connect, &trace).is_err());
        assert_eq!(slot.as_ref().unwrap().id, 3);
        assert!(fake_connect(&mut slot, ImportedPort::Valid(33445), 4, ConnectStep::Nonblocking, &trace).is_err());
        assert_eq!(slot.as_ref().unwrap().id, 4);
        assert_eq!(connection.as_ref().unwrap().id, 0);
        connection =
            Some(fake_connect(&mut slot, ImportedPort::Valid(33445), 5, ConnectStep::Success, &trace).unwrap());
        assert_eq!(connection.as_ref().unwrap().id, 5);
        assert_eq!(
            *trace.lock().unwrap(),
            [
                "create0",
                "connect0:33445",
                "nonblocking0",
                "create1",
                "create2",
                "create3",
                "drop2",
                "connect3:33445",
                "create4",
                "drop3",
                "connect4:33445",
                "nonblocking4",
                "create5",
                "drop4",
                "connect5:33445",
                "nonblocking5",
                "drop0",
            ]
        );
    }

    #[test]
    fn native_tcp_actual_listener_catches_setup_before_queueing_requests() {
        for simulator in [Simulator::Fsx, Simulator::Prepar3d, Simulator::Msfs] {
            let trace = Arc::new(Mutex::new(Vec::new()));
            let mut slot = None;
            let mut attempt = 0;
            let captured = trace.clone();
            let connect = Box::new(move |_| {
                let step = [ConnectStep::Create, ConnectStep::Connect, ConnectStep::Nonblocking, ConnectStep::Success]
                    [attempt];
                let id = attempt;
                attempt += 1;
                fake_connect(&mut slot, ImportedPort::Valid(33445), id, step, &captured).map(FakeNativeStream)
            });
            let source = FlightSimulator::without_receiver(FlightSimSettings::new(simulator), connect);
            let link = source.link.clone();
            let mut listener = crate::derive::SourceListener::new(source);
            for _ in 0..3 {
                assert!(!crate::derive::Listener::is_running(&mut listener));
                assert!(!crate::derive::Listener::check_status_raised(&listener));
                assert_eq!(crate::derive::Listener::native_channel(&listener, "raw_flag"), Some(LegacyValue::Int(0)));
                let link = lock(&link);
                assert!(link.connection.is_none());
                assert_eq!(link.counter, 0, "failed socket setup does not queue any RequestDataRefs message");
            }
            assert!(crate::derive::Listener::is_running(&mut listener));
            assert_eq!(lock(&link).counter as usize, setup_messages(simulator, &simulator.definitions()).len());
            let before = trace.lock().unwrap().clone();
            assert!(crate::derive::Listener::is_running(&mut listener));
            assert_eq!(*trace.lock().unwrap(), before, "connected path does not allocate again");
        }
    }

    fn source(simulator: Simulator) -> FlightSimulator<FakeStream> {
        FlightSimulator::without_receiver(FlightSimSettings::new(simulator), Box::new(|_| Ok(FakeStream::default())))
    }

    #[test]
    fn native_getter_reads_gate_fields_then_current_branch_value() {
        use aircraft::Datum::*;
        let s = source(Simulator::Fsx);
        lock(&s.freeze).observe(-1.0);
        let mut reads = VecDeque::from([
            (BankDegrees, 1.0),
            (PitchDegrees, 2.0),
            (HeadingDegreesTrue, 3.0),
            (AccelerationY, 4.0),
            (AccelerationZ, 5.0),
            (AccelerationX, 6.0),
            (AirspeedIndicated, 123.0),
        ]);
        assert_eq!(
            s.native_answer_with(
                "speed",
                |datum| {
                    let (expected, value) = reads.pop_front().expect("unexpected read");
                    assert_eq!(datum, expected);
                    Some(value)
                },
                || panic!("speed does not read title")
            ),
            Answer::Number(123.0)
        );
        assert!(reads.is_empty());
    }

    #[test]
    fn native_turbulence_rereads_wind_for_individual_previous_assignments() {
        use aircraft::Datum::*;
        let s = source(Simulator::Msfs);
        let mut reads = VecDeque::from([
            (CameraState, Some(2.0)),
            (WindX, Some(3.0)),
            (WindY, Some(0.0)),
            (WindZ, Some(0.0)),
            (WindX, Some(8.0)),
            (WindY, None),
        ]);
        assert_eq!(
            s.native_answer_with(
                "turbulence",
                |datum| {
                    let (expected, value) = reads.pop_front().expect("unexpected read");
                    assert_eq!(datum, expected);
                    value
                },
                || None
            ),
            Answer::Integer(0)
        );
        assert!(reads.is_empty());
        assert_eq!(*lock(&s.previous_wind), [8.0, 0.0, 0.0], "completed first assignment survives second failure");
    }

    #[test]
    fn native_maximum_rechecks_percent_and_preserves_published_prefix_on_failure() {
        use aircraft::Datum::*;
        let s = source(Simulator::Msfs);
        let mut reads = VecDeque::from([
            (CameraState, Some(2.0)),
            (EngineMaxRpmPercent(0), Some(10.0)),
            (EngineRpm(0), Some(1000.0)),
            (EngineMaxRpmPercent(0), Some(20.0)),
            (EngineMaxRpmPercent(1), None),
        ]);
        assert_eq!(
            s.native_answer_with(
                "max_rpm",
                |datum| {
                    let (expected, value) = reads.pop_front().expect("unexpected read");
                    assert_eq!(datum, expected);
                    value
                },
                || None
            ),
            Answer::Integer(0)
        );
        assert!(reads.is_empty());
        assert_eq!(*lock(&s.estimated_rpm), [Some(Answer::Number(500.0)), None, None, None]);
        assert_eq!(
            s.native_answer_with(
                "max_rpm1",
                |datum| {
                    assert_eq!(datum, CameraState);
                    Some(2.0)
                },
                || None
            ),
            Answer::Number(500.0)
        );
    }

    #[test]
    fn decoded_publication_keeps_missing_fields_distinct_from_zero() {
        use aircraft::Datum::*;
        let decoded = aircraft();
        let mut current = AircraftState::empty();
        current.publish(Title, &decoded);
        current.publish(EngineRpm(0), &decoded);
        assert_eq!(current.title, decoded.title);
        assert_eq!(current.number(EngineRpm(0)), Some(2100.0));
        assert_eq!(current.number(EngineRpm(1)), None);
        let s = source(Simulator::Msfs);
        lock(&s.link).aircraft = Some(current);
        assert_eq!(
            s.read_native("speed"),
            NativeRead::Value(LegacyValue::Int(0)),
            "missing camera gate catches before data"
        );
        lock(&s.link).aircraft.as_mut().unwrap().publish(CameraState, &decoded);
        assert_eq!(s.read_native("engine_rpm1"), NativeRead::Value(LegacyValue::Float(2100.0)));
        assert_eq!(s.read_native("engine_rpm2"), NativeRead::Value(LegacyValue::Int(0)));
    }

    #[test]
    fn native_receiver_publishes_successful_block_without_holding_connection_lock() {
        let mut source = source(Simulator::Msfs);
        let mut block = vec![0; 256];
        for definition in source.definitions.iter().skip(1) {
            let value = match definition.datum {
                aircraft::Datum::CameraState => 2.0_f32,
                aircraft::Datum::AirspeedIndicated => 123.5,
                _ => 1.0,
            };
            block.extend(value.to_le_bytes());
        }
        let mut body: Vec<u8> = [1u32, 0, 1, 0, 0, 1, 33].iter().flat_map(|word| word.to_le_bytes()).collect();
        body.extend(block);
        let mut stream = FakeStream::default();
        stream.reads.push_back(Ok(server_frame(8, &body)));
        {
            let mut link = lock(&source.link);
            link.connection = Some(Connection::new(stream));
            link.connected = true;
        }
        source.receiver = source.spawn_receiver();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if source.link.try_lock().ok().is_some_and(|link| {
                link.aircraft.as_ref().and_then(|state| state.number(aircraft::Datum::CameraState)) == Some(2.0)
            }) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "receiver did not release/publish");
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::Float(123.5)));
    }

    #[test]
    fn native_receive_does_not_replay_getters() {
        let mut s = source(Simulator::Fsx);
        {
            let mut link = lock(&s.link);
            link.connected = true;
            link.aircraft = Some(aircraft());
        }
        let mut frame = Telemetry::off();
        assert!(s.receive_native(&mut frame).unwrap());
        assert_eq!(lock(&s.freeze).len, 0, "receiveData only returns connected; getChannel advances the FIFO");
        assert_eq!(*lock(&s.previous_wind), [0.0; 3]);
        assert_eq!(frame.speed, Telemetry::off().speed);
    }

    #[test]
    fn native_missing_output_keys_are_caught_at_each_simulator_boundary() {
        for simulator in [Simulator::Fsx, Simulator::Prepar3d, Simulator::Msfs] {
            let s = source(simulator);
            let mut block = vec![0; 256];
            for definition in s.definitions.iter().skip(1) {
                let value: f32 = if definition.datum == aircraft::Datum::CameraState { 2.0 } else { 0.0 };
                block.extend(value.to_le_bytes());
            }
            let mut a = AircraftState::decode(&s.definitions, &block).unwrap();
            lock(&s.link).aircraft = Some(a.clone());
            assert_eq!(s.read_native("raw_flag"), NativeRead::Value(LegacyValue::Int(0)));
            a.pitch = 1.0;
            lock(&s.link).aircraft = Some(a);
            for name in ["raw_flag", "gear", "flag", "not_in_output", "max_rpm1"] {
                assert_eq!(s.read_native(name), NativeRead::Value(LegacyValue::Int(0)), "{simulator:?} {name}");
            }
            assert_eq!(
                s.read_native("camera_state"),
                NativeRead::Value(if simulator == Simulator::Msfs {
                    LegacyValue::Float(2.0)
                } else {
                    LegacyValue::Int(0)
                })
            );
        }
    }

    #[test]
    fn native_caught_missing_keys_reach_dispatcher_as_zero_instead_of_defaults() {
        use crate::derive::*;
        struct Games;
        impl GameDirectory for Games {
            fn is_flying(&self, _: &str) -> bool {
                true
            }
            fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
                None
            }
        }
        for simulator in [Simulator::Fsx, Simulator::Prepar3d, Simulator::Msfs] {
            let s = source(simulator);
            lock(&s.link).aircraft = Some(aircraft());
            lock(&s.link).connected = true;
            if simulator != Simulator::Msfs {
                s.read_native("speed");
                lock(&s.link).aircraft.as_mut().unwrap().pitch += 1.0;
            }
            let id = s.id().to_owned();
            let mut dispatcher = Dispatcher::new(vec![Box::new(SourceListener::new(s))]);
            let settings = DispatchSettings {
                dams: DamsSettings {
                    mode: DamsMode::Off,
                    factor: Some(1.0),
                    front_lever: 0.0,
                    rear_lever: 1.44,
                    steering_ratio: Some(14.5),
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
            let hits = [ScanHit { game_id: id.clone(), listener_id: id }];
            dispatcher.tick(&TickInput {
                hits: &hits,
                settings: &settings,
                profile: &profile,
                runtime: &RuntimeInputs::default(),
                games: &Games,
            });
            assert!(!dispatcher.has_ended());
            for name in ["raw_flag", "gear", "flag"] {
                assert_eq!(
                    dispatcher.telemetry().native_value(name),
                    Some(&LegacyValue::Int(0)),
                    "{simulator:?} {name}"
                );
            }
        }
    }

    #[test]
    fn native_getters_count_actual_requests_and_observe_new_data() {
        for simulator in [Simulator::Fsx, Simulator::Prepar3d] {
            let s = source(simulator);
            assert_eq!(s.read_native("speed"), NativeRead::Value(LegacyValue::Int(0)));
            assert_eq!(lock(&s.freeze).len, 0, "an empty output does not enter the FIFO");
            lock(&s.link).aircraft = Some(aircraft());
            assert_eq!(s.read_native("speed"), NativeRead::Value(LegacyValue::Int(0)));
            let mut moving = aircraft();
            moving.pitch += 1.0;
            moving.airspeed = 101.0;
            lock(&s.link).aircraft = Some(moving.clone());
            assert_eq!(s.read_native("speed"), NativeRead::Value(LegacyValue::Float(101.0)));
            moving.airspeed = 102.0;
            lock(&s.link).aircraft = Some(moving);
            assert_eq!(s.read_native("speed"), NativeRead::Value(LegacyValue::Float(102.0)));
            for _ in 0..997 {
                assert_eq!(s.read_native("missing"), NativeRead::Value(LegacyValue::Int(0)));
            }
            assert_eq!(lock(&s.freeze).len, FREEZE_WINDOW);
            assert_eq!(s.read_native("missing"), NativeRead::Value(LegacyValue::Int(0)));
        }
    }

    #[test]
    fn native_measurements_defaults_and_first_maximum_kinds() {
        let s = source(Simulator::Msfs);
        lock(&s.link).aircraft = Some(aircraft());
        for (name, value) in [
            ("speed", LegacyValue::Float(95.5)),
            ("rpm", LegacyValue::Float(2300.0)),
            ("max_rpm", LegacyValue::Float(460.0)),
            ("yaw_rate", LegacyValue::Float(2.0)),
            ("pitch_rate", LegacyValue::Float(-1.0)),
            ("roll_rate", LegacyValue::Float(-3.0)),
            ("touchdown_nose", LegacyValue::Float(9.0)),
            ("touchdown_left", LegacyValue::Int(0)),
            ("afterburner", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.read_native(name), NativeRead::Value(value), "{name}");
        }
        assert_eq!(s.read_native("track_loc"), NativeRead::Value(LegacyValue::Int(0)));
        let mut a = aircraft();
        a.title = vec![0xff];
        a.vertical_acceleration = 0.0;
        a.engine_rpm = [0.0; 4];
        a.engine_max_rpm_percent = [0.0, 1.0, 1.0, 1.0];
        lock(&s.link).aircraft = Some(a.clone());
        assert_eq!(s.read_native("car_name"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(s.read_native("touchdown_nose"), NativeRead::Value(LegacyValue::Float(0.0)));
        assert_eq!(s.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(0)));
        a.engine_max_rpm_percent = [1.0, 0.0, 0.0, 0.0];
        lock(&s.link).aircraft = Some(a.clone());
        assert_eq!(s.read_native("max_rpm"), NativeRead::Value(LegacyValue::Float(0.0)));
        a.engine_rpm[0] = f64::NAN;
        lock(&s.link).aircraft = Some(a);
        assert!(matches!(s.read_native("rpm"), NativeRead::Value(LegacyValue::Float(n)) if n.is_nan()));
        assert!(matches!(s.read_native("max_rpm"), NativeRead::Value(LegacyValue::Float(n)) if n.is_nan()));
    }

    #[test]
    fn native_turbulence_updates_only_on_an_open_turbulence_read() {
        let mut s = source(Simulator::Msfs);
        let mut a = aircraft();
        a.wind = [3.0, 0.0, 0.0];
        lock(&s.link).aircraft = Some(a.clone());
        s.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(s.read_native("speed"), NativeRead::Value(LegacyValue::Float(95.5)));
        assert_eq!(*lock(&s.previous_wind), [0.0; 3]);
        assert_eq!(s.read_native("turbulence"), NativeRead::Value(LegacyValue::Float(3.0)));
        assert_eq!(s.read_native("turbulence"), NativeRead::Value(LegacyValue::Int(0)));
        a.wind = [3.5, 0.0, 0.0];
        lock(&s.link).aircraft = Some(a.clone());
        assert_eq!(s.read_native("turbulence"), NativeRead::Value(LegacyValue::Int(0)));
        a.wind = [5.0, 0.0, 0.0];
        a.camera_state = 7.0;
        lock(&s.link).aircraft = Some(a.clone());
        assert_eq!(s.read_native("turbulence"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(*lock(&s.previous_wind), [3.5, 0.0, 0.0]);
        a.camera_state = 2.0;
        lock(&s.link).aircraft = Some(a);
        assert_eq!(s.read_native("turbulence"), NativeRead::Value(LegacyValue::Float(1.5)));
    }

    #[test]
    fn native_raw_dictionary_keys_and_maximum_cache_survive_new_frames() {
        for simulator in [Simulator::Fsx, Simulator::Prepar3d, Simulator::Msfs] {
            let s = source(simulator);
            // An older, different sum keeps the FSX/P3D pause gate open during these getter queries.
            lock(&s.freeze).observe(-999.0);
            lock(&s.link).aircraft = Some(aircraft());
            for (name, value) in [
                ("engine_rpm1", 2100.0),
                ("engine_rpm4", 0.0),
                ("engine_max_perc_rpm1", 70.0),
                ("engine_max_perc_rpm2", 0.0),
                ("lateral_wind", 1.0),
                ("vertical_wind", 1.0),
                ("longitudinal_wind", 1.0),
                ("afterburner_1", 1.0),
                ("landing_gear_center", 100.0),
                ("gear_animation_center", 60.0),
                ("gear_animation_left", 40.0),
                ("gear_animation_right", 100.0),
                ("gear_animation_aux", 70.0),
                ("is_onground", 1.0),
            ] {
                assert_eq!(s.read_native(name), NativeRead::Value(LegacyValue::Float(value)), "{name}");
            }
            assert_eq!(s.read_native("afterburner_0"), NativeRead::Value(LegacyValue::Int(0)));
            assert_eq!(s.read_native("max_rpm1"), NativeRead::Value(LegacyValue::Int(0)));
            assert_eq!(s.read_native("max_rpm"), NativeRead::Value(LegacyValue::Float(460.0)));
            assert_eq!(s.read_native("max_rpm1"), NativeRead::Value(LegacyValue::Float(300.0)));
            assert_eq!(s.read_native("max_rpm2"), NativeRead::Value(LegacyValue::Int(0)));
            lock(&s.link).aircraft.as_mut().unwrap().engine_rpm[0] = 4200.0;
            assert_eq!(s.read_native("engine_rpm1"), NativeRead::Value(LegacyValue::Float(4200.0)));
            assert_eq!(s.read_native("max_rpm1"), NativeRead::Value(LegacyValue::Float(300.0)));
            assert_eq!(s.read_native("max_rpm"), NativeRead::Value(LegacyValue::Float(600.0)));
            assert_eq!(s.read_native("max_rpm1"), NativeRead::Value(LegacyValue::Float(600.0)));
            assert_eq!(
                s.read_native("camera_state"),
                if simulator == Simulator::Msfs {
                    NativeRead::Value(LegacyValue::Float(2.0))
                } else {
                    NativeRead::Value(LegacyValue::Int(0))
                }
            );
        }
    }

    #[test]
    fn native_menu_model_reader_tracks_each_getter() {
        use crate::derive::{Listener, Rate, SourceListener};
        use std::sync::atomic::AtomicUsize;

        let mut s = source(Simulator::Msfs);
        let mut a = aircraft();
        a.camera_state = 7.0;
        lock(&s.link).aircraft = Some(a.clone());
        let model = Arc::new(Mutex::new(Some(LegacyValue::from("Yaw2 PRO"))));
        let reads = Arc::new(AtomicUsize::new(0));
        let model_copy = model.clone();
        let reads_copy = reads.clone();
        s.set_model_reader(Arc::new(move || {
            reads_copy.fetch_add(1, Ordering::SeqCst);
            lock(&model_copy).clone()
        }));
        let listener = SourceListener::new(s);
        assert_eq!(listener.read_native("yaw"), NativeRead::Value(LegacyValue::Float(a.yaw)));
        *lock(&model) = Some(LegacyValue::from("None"));
        assert_eq!(listener.rate(Rate::Yaw), Some(0.0));
        *lock(&model) = Some(LegacyValue::from("Yaw3"));
        assert_eq!(listener.rate(Rate::Yaw), Some(a.yaw_rate));
        for value in [None, Some(LegacyValue::from("unknown")), Some(LegacyValue::Bool(true))] {
            *lock(&model) = value;
            assert_eq!(listener.read_native("yaw"), NativeRead::Value(LegacyValue::Int(0)));
        }
        *lock(&model) = Some(LegacyValue::from("Yaw2 PRO"));
        assert_eq!(listener.read_native("speed"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            7,
            "every menu read resolves the current model before channel selection"
        );
    }

    #[test]
    fn native_menu_model_reader_is_skipped_outside_msfs_menus() {
        for simulator in [Simulator::Msfs, Simulator::Fsx, Simulator::Prepar3d] {
            let mut s = source(simulator);
            s.set_model_reader(Arc::new(|| panic!("model read outside the MSFS menu branch")));
            assert_eq!(s.read_native("yaw"), NativeRead::Value(LegacyValue::Int(0)));
            let mut a = aircraft();
            a.camera_state = 2.0;
            lock(&s.link).aircraft = Some(a.clone());
            s.read_native("yaw");
            a.pitch += 1.0;
            lock(&s.link).aircraft = Some(a.clone());
            assert_eq!(s.read_native("yaw"), NativeRead::Value(LegacyValue::Float(a.yaw)));
            if simulator == Simulator::Msfs {
                for camera in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                    a.camera_state = camera;
                    lock(&s.link).aircraft = Some(a.clone());
                    assert_eq!(s.read_native("yaw"), NativeRead::Value(LegacyValue::Int(0)));
                }
            }
        }
    }

    #[test]
    fn source_adapter_reads_live_aircraft_and_rates_without_replaying_a_tick() {
        use crate::derive::{Listener, Rate, SourceListener};
        let s = source(Simulator::Msfs);
        lock(&s.link).aircraft = Some(aircraft());
        let shared = Arc::clone(&s.link);
        let mut listener = SourceListener::new(s);
        listener.receive();
        lock(&shared).aircraft.as_mut().unwrap().airspeed = 102.0;
        assert_eq!(listener.read_native("speed"), NativeRead::Value(LegacyValue::Float(102.0)));
        assert_eq!(listener.rate(Rate::Yaw), Some(2.0));
        assert_eq!(listener.read_native("turbulence"), NativeRead::Value(LegacyValue::Float(3.0_f64.sqrt())));
        assert_eq!(listener.read_native("turbulence"), NativeRead::Value(LegacyValue::Int(0)));
        lock(&shared).aircraft.as_mut().unwrap().camera_state = 7.0;
        assert_eq!(listener.rate(Rate::Yaw), Some(0.0));
        assert_eq!(listener.read_native("track_loc"), NativeRead::Value(LegacyValue::Int(0)));
    }

    fn aircraft() -> AircraftState {
        AircraftState {
            title: b"Cessna Skyhawk\0\0\0".to_vec(),
            engine_rpm: [2100.0, 2300.0, 2300.0, 0.0],
            engine_max_rpm_percent: [70.0, 0.0, 50.0, 0.0],
            airspeed: 95.5,
            pitch: 5.0,
            roll: -100.0,
            yaw: 270.0,
            pitch_rate: 1.0,
            yaw_rate: 2.0,
            roll_rate: 3.0,
            lateral_acceleration: 20.0,
            vertical_acceleration: 9.0,
            longitudinal_acceleration: 6.0,
            wind: [1.0, 1.0, 1.0],
            canopy: 40.0,
            afterburner_1: 1.0,
            landing_gear: [100.0, 90.0, 80.0],
            gear_animation: [60.0, 40.0, 100.0],
            gear_animation_aux: 70.0,
            on_ground: 1.0,
            camera_state: 2.0,
            present: None,
        }
    }

    /// The client messages written to the stream, as (code, protocol, counter).
    fn sent_headers(bytes: &[u8]) -> Vec<(u32, u32, u32)> {
        let mut headers = Vec::new();
        let mut at = 0;
        let word = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        while at < bytes.len() {
            headers.push((word(at + 8) & 0x0FFF_FFFF, word(at + 4), word(at + 12)));
            at += word(at) as usize;
        }
        headers
    }

    fn drain(source: &FlightSimulator<FakeStream>) -> Vec<u8> {
        let mut link = lock(&source.link);
        for _ in 0..16 {
            link.receive_once(&source.definitions);
        }
        link.connection.as_ref().unwrap().stream().written.clone()
    }

    /// Set-up messages: open, system state, SimStart, 32 definitions, the data request, then the
    /// stop-effects event mapping, notification group and its priority; counters keep running across sessions.
    #[test]
    fn session_setup_sequence() {
        let mut fsx = source(Simulator::Fsx);
        assert!(fsx.is_running().unwrap());
        let headers = sent_headers(&drain(&fsx));
        let codes: Vec<u32> = headers.iter().map(|h| h.0).collect();
        let mut expected = vec![1, 53, 23];
        expected.extend([12; 32]);
        expected.extend([14, 4, 7, 9]);
        assert_eq!(codes, expected);
        assert!(headers.iter().all(|h| h.1 == 4));
        assert_eq!(headers.iter().map(|h| h.2).collect::<Vec<_>>(), (0..39).collect::<Vec<_>>());
        lock(&fsx.link).connected = false;
        assert!(fsx.is_running().unwrap());
        assert_eq!(sent_headers(&drain(&fsx))[0].2, 39);

        let mut p3d = source(Simulator::Prepar3d);
        assert!(p3d.is_running().unwrap());
        let bytes = drain(&p3d);
        assert!(sent_headers(&bytes).iter().all(|h| h.1 == 29));
        assert_eq!(&bytes[16..25], b"SRSLogger");
        let mut msfs = source(Simulator::Msfs);
        assert!(msfs.is_running().unwrap());
        assert_eq!(sent_headers(&drain(&msfs)).len(), 40);
    }

    /// A refused connection is "not running".
    #[test]
    fn refused_connection() {
        let mut s = FlightSimulator::without_receiver(
            FlightSimSettings::new(Simulator::Fsx),
            Box::new(|_| Err::<FakeStream, _>(io::ErrorKind::ConnectionRefused.into())),
        );
        assert!(!s.is_running().unwrap());
    }

    /// A data message decodes the block; a short block drops the connection and the next check reconnects.
    #[test]
    fn receive_loop_messages() {
        let mut s = source(Simulator::Prepar3d);
        assert!(s.is_running().unwrap());
        let mut block = vec![0u8; 32];
        block.extend((0..31).flat_map(|i| (i as f32).to_le_bytes()));
        let mut body: Vec<u8> = [1u32, 0, 1, 0, 0, 1, 32].iter().flat_map(|w| w.to_le_bytes()).collect();
        body.extend_from_slice(&block);
        let mut stream = FakeStream::default();
        stream.reads.push_back(Ok(server_frame(8, &body)));
        stream.reads.push_back(Ok(server_frame(8, &body[..40])));
        lock(&s.link).connection = Some(Connection::new(stream));
        lock(&s.link).receive_once(&s.definitions);
        assert_eq!(lock(&s.link).aircraft.as_ref().map(|a| a.airspeed), Some(8.0));
        assert!(lock(&s.link).connected);
        lock(&s.link).receive_once(&s.definitions);
        assert!(!lock(&s.link).connected);
        // The decoded state survives the reconnect.
        assert!(s.is_running().unwrap());
        assert!(lock(&s.link).aircraft.is_some());
    }

    /// The FSX listener alone prints the failed FlyInside registration, each time it queues the set-up messages.
    #[test]
    fn fsx_prints_the_failed_flyinside_registration() {
        for (simulator, expected) in [
            (Simulator::Fsx, vec![FLYINSIDE_REGISTRATION_FAILURE.to_owned()]),
            (Simulator::Prepar3d, vec![]),
            (Simulator::Msfs, vec![]),
        ] {
            let mut s = source(simulator);
            crate::printed::take();
            assert!(s.is_running().unwrap());
            assert_eq!(crate::printed::take(), expected);
        }
        assert_eq!(
            FLYINSIDE_REGISTRATION_FAILURE,
            "Could not register FlyInside client data: module 'backend.listeners.simconnect.message' has no \
             attribute 'CMapClientDataNameToID'"
        );
    }

    /// Receive loop: a message without a handler is printed with its member list; whatever failure the pump or a
    /// handler reports is printed after `Exception 2` and drops the connection flag.
    #[test]
    fn receive_loop_prints_what_the_original_prints() {
        let s = source(Simulator::Prepar3d);
        let words = |values: &[u32]| values.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let printed = |reads: Vec<std::io::Result<Vec<u8>>>| {
            let mut stream = FakeStream::default();
            stream.reads.extend(reads);
            let mut link = lock(&s.link);
            link.connection = Some(Connection::new(stream));
            link.connected = true;
            crate::printed::take();
            link.receive_once(&s.definitions);
            (crate::printed::take(), link.connected)
        };
        assert_eq!(
            printed(vec![Ok(server_frame(1, &words(&[7, 21, 3])))]),
            (vec!["ERROR: no handler for <SException: exception: 7, sendID: 21, index: 3>".to_owned()], true)
        );
        assert_eq!(printed(vec![Ok(server_frame(3, &[]))]), (vec!["ERROR: no handler for <SQuit: >".to_owned()], true));
        // An event matches no client event entry and prints nothing.
        assert_eq!(printed(vec![Ok(server_frame(4, &words(&[0, 3, 0, 0])))]), (vec![], true));
        assert_eq!(
            printed(vec![Ok(server_frame(8, &words(&[1, 0, 1, 0, 0, 1, 32])))]),
            (vec!["Exception 2 unpack requires a buffer of 32 bytes".to_owned()], false)
        );
        assert_eq!(printed(vec![Ok(server_frame(6, &[]))]), (vec!["Exception 2 ('s', 6)".to_owned()], false));
        assert_eq!(printed(vec![Ok(server_frame(4, &words(&[0, 3, 0])))]), (vec!["Exception 2 ".to_owned()], false));
        let mut high_bits = server_frame(3, &[]);
        high_bits[11] = 0x10;
        assert_eq!(printed(vec![Ok(high_bits)]), (vec!["Exception 2 ENCODING FAILURE".to_owned()], false));
        let (closed, connected) = printed(vec![Err(std::io::ErrorKind::ConnectionReset.into())]);
        assert!(!connected);
        let [closed] = &closed[..] else { panic!("{closed:?}") };
        let address = closed
            .strip_prefix("Exception 2 <backend.listeners.simconnect.connection.Connection object at 0x")
            .and_then(|rest| rest.strip_suffix('>'))
            .unwrap_or_else(|| panic!("{closed}"));
        assert!(!address.is_empty() && address.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// The dispatcher's processed tick asks 78 times: 8 dedicated reads, the 66 auto-loaded channels (including
    /// harmonized_gear), then yaw rate, yaw, pitch and roll.
    #[test]
    fn dispatcher_call_order() {
        let calls = dispatcher_calls();
        assert_eq!(calls.len(), 78);
        assert_eq!(calls[8].request, "lateral_acceleration");
        assert!(calls.iter().any(|c| c.request == "harmonized_gear"));
        assert_eq!(calls.iter().filter(|c| c.request == "speed").count(), 3);
        assert_eq!(calls[77].request, "roll");
    }

    /// The pause window of the FSX and Prepar3D listeners:
    /// a single entry or a window holding one value reads as paused; NaN equals nothing.
    #[test]
    fn freeze_detector_window() {
        let mut freeze = FreezeDetector::default();
        assert!(freeze.observe(1.0));
        assert!(freeze.observe(1.0));
        assert!(!freeze.observe(2.0));
        for _ in 0..998 {
            assert!(!freeze.observe(2.0));
        }
        assert!(freeze.observe(2.0));
        assert!(!freeze.observe(f64::NAN));
        assert!(FreezeDetector::default().observe(f64::NAN));
    }

    /// FSX channel reads over the dispatcher's calls: everything 0 before data and on the first sample, then
    /// the mapped values; unmapped channels read integer zero.
    #[test]
    fn tick_answers_fsx() {
        let mut s = source(Simulator::Fsx);
        let mut t = Telemetry::off();
        s.evaluate_tick(None, &mut t);
        assert_eq!((t.gear, t.rpm, t.max_rpm), (0.0, 0.0, 0.0));
        assert_eq!(s.angular_rates(), AngularRates::default());
        let first = aircraft();
        s.evaluate_tick(Some(&first), &mut t);
        assert_eq!((t.speed, t.pitch, t.vehicle.as_str()), (0.0, 0.0, ""));
        let mut moving = aircraft();
        moving.pitch = 6.0;
        moving.wind = [3.0, 1.0, 1.0];
        t.gear = 3.0;
        s.evaluate_tick(Some(&moving), &mut t);
        assert_eq!(t.gear, 0.0);
        assert_eq!(t.speed, 95.5);
        assert_eq!((t.pitch, t.roll, t.yaw), (-6.0, 80.0, 270.0));
        assert_eq!((t.lateral_acceleration, t.vertical_acceleration, t.longitudinal_acceleration), (2.0, 3.0, 2.0));
        assert_eq!(t.rpm, 2300.0);
        assert_eq!(t.max_rpm, 2300.0 * 10.0 / 50.0);
        // The previous wind starts at 0 and only moves when the turbulence request is answered.
        assert_eq!(t.turbulence, 11.0_f64.sqrt());
        assert_eq!((t.touchdown_nose, t.touchdown_left, t.touchdown_right), (9.0, 0.0, 9.0));
        assert_eq!((t.canopy, t.afterburner), (40.0, 0.0));
        assert_eq!((t.landing_gear_nose, t.landing_gear_left, t.landing_gear_right), (100.0, 90.0, 80.0));
        assert_eq!(t.vehicle, "Cessna Skyhawk\0\0\0");
        assert_eq!(s.angular_rates(), AngularRates { yaw: 2.0, pitch: -1.0, roll: -3.0 });
        // Same values again: the wind did not change, so no turbulence; the FIFO still holds the older sum.
        s.evaluate_tick(Some(&moving), &mut t);
        assert_eq!((t.turbulence, t.speed), (0.0, 95.5));
    }

    /// A missing output entry is handled by the listener before the dispatcher sees it, so
    /// even default-None channels and raw_flag receive zero instead of retaining stale values.
    #[test]
    fn raising_calls_write_dispatcher_defaults() {
        let mut s = source(Simulator::Fsx);
        let mut t = Telemetry::off();
        let first = aircraft();
        s.evaluate_tick(Some(&first), &mut t);
        let mut moving = aircraft();
        moving.pitch = 6.0;
        // Stale values, e.g. Telemetry::off()'s steering_wheel_angle 30 or another source's.
        (t.steering_wheel_angle, t.raw_flag) = (30.0, 0.0);
        (t.gear, t.flag, t.lateral_velocity, t.tyre_wear_lf, t.lap) = (3.0, 2.0, 7.0, 0.5, 4.0);
        s.evaluate_tick(Some(&moving), &mut t);
        assert_eq!((t.steering_wheel_angle, t.raw_flag), (0.0, 0.0));
        assert_eq!((t.tyre_wear_lf, t.lap, t.lateral_velocity), (0.0, 0.0, 0.0));
        assert_eq!((t.gear, t.flag), (0.0, 0.0));
    }

    /// p3d_invert_* factors flip the attitude and rate signs.
    #[test]
    fn inversion_factors() {
        let mut settings = FlightSimSettings::new(Simulator::Prepar3d);
        settings.inversion = AttitudeInversion { yaw: true, roll: true, pitch: true };
        let s = FlightSimulator::without_receiver(settings, Box::new(|_| Ok(FakeStream::default())));
        let a = aircraft();
        assert_eq!(s.answer("pitch", &a), Answer::Number(5.0));
        assert_eq!(s.answer("yaw", &a), Answer::Number(-270.0));
        assert_eq!(s.answer("roll_rate", &a), Answer::Number(3.0));
        assert_eq!(s.answer("gear", &a), Answer::Raise);
    }

    /// The vehicle names of FSX / MSFS (decode, strip, "Asobo", "paint" suffix) and Prepar3D.
    #[test]
    fn vehicle_names() {
        let fsx = source(Simulator::Fsx);
        let mut title = b"Asobo Cessna 172 Paint3".to_vec();
        title.resize(256, 0);
        assert_eq!(fsx.vehicle_name(&title), Answer::Text(" Cessna 172".into()));
        assert_eq!(fsx.vehicle_name(b"  Extra 330  "), Answer::Text("Extra 330".into()));
        assert_eq!(fsx.vehicle_name(b"Caravan paint"), Answer::Text("Caravan".into()));
        assert_eq!(fsx.vehicle_name(&[0xff, 0x41]), Answer::Integer(0));
        let p3d = source(Simulator::Prepar3d);
        assert_eq!(p3d.vehicle_name(b"Boeing 737   \0garbage"), Answer::Text("Boeing 737".into()));
    }

    /// MSFS: camera state above 6 reads 0, except yaw / yaw_rate for a Yaw VR platform.
    #[test]
    fn msfs_camera_gate() {
        let mut settings = FlightSimSettings::new(Simulator::Msfs);
        settings.yaw_vr = true;
        let mut s = FlightSimulator::without_receiver(settings, Box::new(|_| Ok(FakeStream::default())));
        let mut menu = aircraft();
        menu.camera_state = 7.0;
        let mut t = Telemetry::off();
        s.evaluate_tick(Some(&menu), &mut t);
        assert_eq!((t.yaw, t.pitch, t.speed), (270.0, 0.0, 0.0));
        assert_eq!(s.angular_rates(), AngularRates { yaw: 2.0, pitch: 0.0, roll: 0.0 });
        menu.camera_state = 6.9;
        s.evaluate_tick(Some(&menu), &mut t);
        assert_eq!((t.pitch, t.speed), (-5.0, 95.5));
        menu.camera_state = f64::NAN;
        s.evaluate_tick(Some(&menu), &mut t);
        assert_eq!(t.yaw, 0.0);
    }
}

//! Assetto Corsa on PlayStation 4, "Remote Telemetry" UDP protocol (listener `acudp`).
//!
//! A background worker broadcasts the handshake to port 9996, then receives on its own port: the 408-byte
//! handshake response is answered with a "subscribe update" request and every 328-byte `RTCarInfo` update
//! becomes the current car state. The dispatcher only reads the state the worker keeps.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::forward::{Forwarder, PortForwarding};
use super::parameters::Parameters;
use super::project_motor_racing::NetworkPortsReader;
use super::socket::ImportedPort;
pub use super::socket::ListenerSettings;
use super::support::channels::zero_listener_channels;
use super::support::net::{Datagrams, Endpoint, Network, SystemNetwork};
use super::support::wire::{Fields, Short};
use crate::{NativeRead, Source, SourceError};

/// Configuration default used by compatibility fixtures. The worker reads the port per session.
#[cfg(test)]
const DEFAULT_PORT: u16 = 9997;
/// The listener and catalog id the worker compares the running game with.
const GAME_ID: &str = "acudp";
const RECEIVE_SIZE: usize = 8192;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
/// The two-second pause: once after a failed session, and once at the end of every loop iteration.
const LOOP_PAUSE: Duration = Duration::from_secs(2);
const HANDSHAKE_RESPONSE_LEN: usize = 408;
const CAR_INFO_LEN: usize = 328;
/// Handshake requests are broadcast to the console's remote telemetry port.
const CONSOLE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::BROADCAST, 9996);
const MAX_RPM: f64 = 10000.0;

/// A request: three `i32` (identifier 1, version 1, operation).
fn request(operation: i32) -> Vec<u8> {
    [1i32, 1, operation].iter().flat_map(|v| v.to_le_bytes()).collect()
}
const HANDSHAKE: i32 = 0;
const SUBSCRIBE_UPDATE: i32 = 1;

/// The fields of `RTCarInfo` (C struct
/// alignment, 328 bytes) that the listener uses.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CarInfo {
    speed_ms: f64,
    acc_g_vertical: f64,
    acc_g_horizontal: f64,
    acc_g_frontal: f64,
    engine_rpm: f64,
    steer: f64,
    gear: i32,
    /// Front-left, front-right, rear-left, rear-right.
    suspension_height: [f64; 4],
}

impl CarInfo {
    fn parse(data: &[u8]) -> Result<Self, Short> {
        let f = Fields::new(data);
        if f.len() != CAR_INFO_LEN {
            return Err(Short);
        }
        Ok(CarInfo {
            speed_ms: f.f32(16)?,
            acc_g_vertical: f.f32(28)?,
            acc_g_horizontal: f.f32(32)?,
            acc_g_frontal: f.f32(36)?,
            engine_rpm: f.f32(68)?,
            steer: f.f32(72)?,
            gear: f.i32(76)?,
            suspension_height: [f.f32(292)?, f.f32(296)?, f.f32(300)?, f.f32(304)?],
        })
    }
}

/// What the worker shares with the dispatcher (the connected flag and the car state).
#[derive(Debug, Default)]
struct Shared {
    connected: bool,
    car_info: Option<CarInfo>,
}

/// What one receive step of a session leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Keep receiving.
    Continue,
    /// The receive failed: not connected, pause, then a new session.
    SessionLost,
    /// The subscribe request could not be sent: the worker ends on the unhandled error, leaving
    /// the connected flag as it was.
    WorkerEnds,
}

/// One handshake/receive session of the worker.
struct Session {
    socket: Box<dyn Datagrams>,
}

impl Session {
    fn open_native(
        net: &mut dyn Network,
        read_port: &mut dyn FnMut() -> std::io::Result<ImportedPort>,
    ) -> std::io::Result<Self> {
        let endpoint =
            Endpoint { reuse_address: true, broadcast: true, timeout: None, ..Endpoint::any(0, RECEIVE_TIMEOUT) };
        let mut socket = net.bind_live(&endpoint, read_port)?;
        socket.send_to(&request(HANDSHAKE), CONSOLE.into())?;
        socket.set_read_timeout(Some(RECEIVE_TIMEOUT))?;
        Ok(Session { socket })
    }

    /// Socket with `SO_REUSEADDR` and `SO_BROADCAST`, bound to all interfaces, then the broadcast handshake.
    fn open(net: &mut dyn Network, port: u16) -> std::io::Result<Self> {
        let endpoint = Endpoint { reuse_address: true, broadcast: true, ..Endpoint::any(port, RECEIVE_TIMEOUT) };
        let mut socket = net.bind(&endpoint)?;
        socket.send_to(&request(HANDSHAKE), CONSOLE.into())?;
        Ok(Session { socket })
    }

    /// One iteration of the receive loop: receive, forward, mark connected, then answer the handshake
    /// response with the subscribe request or store a car update.
    fn step(&mut self, forwarder: &Forwarder, shared: &Mutex<Shared>) -> Step {
        let data = match self.socket.receive(RECEIVE_SIZE) {
            Ok((data, _)) => data,
            Err(_) => {
                shared.lock().expect("acudp state").connected = false;
                return Step::SessionLost;
            }
        };
        forwarder.forward(&data);
        {
            // The connected flag and, for a car update, the new car state are published
            // together: the dispatcher sees both or neither.
            let mut shared = shared.lock().expect("acudp state");
            shared.connected = true;
            if data.len() == CAR_INFO_LEN {
                match CarInfo::parse(&data) {
                    Ok(info) => shared.car_info = Some(info),
                    Err(_) => tracing::debug!("error"),
                }
            }
        }
        if data.len() == HANDSHAKE_RESPONSE_LEN {
            if let Err(e) = self.socket.send_to(&request(SUBSCRIBE_UPDATE), CONSOLE.into()) {
                tracing::warn!("acudp: receive thread stopped: {e}");
                return Step::WorkerEnds;
            }
        }
        Step::Continue
    }
}

enum SessionPort {
    Numeric(u16),
    Native(NetworkPortsReader),
}

impl SessionPort {
    fn open_selected(&self, net: &mut dyn Network, control: &Control) -> std::io::Result<Option<Session>> {
        if !control.game_selected() {
            return Ok(None);
        }
        self.open(net).map(Some)
    }

    fn open(&self, net: &mut dyn Network) -> std::io::Result<Session> {
        match self {
            Self::Numeric(port) => Session::open(net, *port),
            Self::Native(reader) => Session::open_native(net, &mut || native_port(reader)),
        }
    }
}

fn native_port(reader: &NetworkPortsReader) -> std::io::Result<ImportedPort> {
    use crate::udp_flight::python::{parse_int_text_checked, IntTextError, PyInt};
    use std::io;
    let values = reader();
    let value = values.get(GAME_ID).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "'acudp'"))?;
    let invalid = || {
        let shown: String = openrigcue_model::python_repr::repr(value).chars().take(200).collect();
        io::Error::new(io::ErrorKind::InvalidInput, format!("invalid literal for int() with base 10: {shown}"))
    };
    let text = match value {
        LegacyValue::Str(text) => Some(text.as_str()),
        LegacyValue::Bytes(bytes) => Some(std::str::from_utf8(bytes).map_err(|_| invalid())?),
        _ => None,
    };
    if let Some(text) = text {
        if matches!(value, LegacyValue::Bytes(bytes) if !bytes.is_ascii()) {
            return Err(invalid());
        }
        return match parse_int_text_checked(text) {
            Ok(PyInt::Small(port)) => Ok(u16::try_from(port).map_or(ImportedPort::OutOfRange, ImportedPort::Valid)),
            Ok(PyInt::Large) => Ok(ImportedPort::OutOfRange),
            Err(IntTextError::Invalid) => Err(invalid()),
            Err(error) => Err(io::Error::new(io::ErrorKind::InvalidInput, error.to_string())),
        };
    }
    ImportedPort::from_native(value).ok_or_else(|| {
        let message = match value {
            LegacyValue::Float(value) if value.is_nan() => "cannot convert float NaN to integer".to_owned(),
            LegacyValue::Float(_) => "cannot convert float infinity to integer".to_owned(),
            other => format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                other.type_name()
            ),
        };
        io::Error::new(io::ErrorKind::InvalidInput, message)
    })
}

/// Sleep in short slices so `stop` is honoured promptly.
fn pause(stop: &AtomicBool, duration: Duration) {
    let slice = Duration::from_millis(50);
    let mut left = duration;
    while !left.is_zero() && !stop.load(Ordering::Relaxed) {
        let d = left.min(slice);
        std::thread::sleep(d);
        left -= d;
    }
}

/// What the worker reads besides the network.
#[derive(Debug)]
struct Control {
    /// The source is being dropped.
    stop: AtomicBool,
    /// The shared parameters: the worker tests whether the running game is `acudp` before each session.
    parameters: Arc<Parameters>,
}

impl Control {
    fn new(parameters: Arc<Parameters>) -> Self {
        Control { stop: AtomicBool::new(false), parameters }
    }

    /// Whether the running game is `acudp`, read when the loop comes round.
    fn game_selected(&self) -> bool {
        self.parameters.game_running_is_one_of(&[GAME_ID])
    }
}

/// The worker loop: while the running game is `acudp` a session
/// (socket, broadcast handshake, receive loop) runs until a receive fails, followed by a two-second
/// pause; every iteration ends with another two-second pause. A failure while opening a session or sending the
/// subscribe request ends the worker.
fn worker(
    mut net: Box<dyn Network>,
    port: SessionPort,
    forwarder: Arc<Forwarder>,
    shared: Arc<Mutex<Shared>>,
    control: Arc<Control>,
) {
    while !control.stop.load(Ordering::Relaxed) {
        let session = match port.open_selected(net.as_mut(), &control) {
            Ok(session) => session,
            Err(e) => {
                tracing::warn!("acudp: receive thread stopped: {e}");
                return;
            }
        };
        if let Some(mut session) = session {
            while !control.stop.load(Ordering::Relaxed) {
                match session.step(&forwarder, &shared) {
                    Step::Continue => {}
                    Step::SessionLost => break,
                    Step::WorkerEnds => return,
                }
            }
            pause(&control.stop, LOOP_PAUSE);
        }
        pause(&control.stop, LOOP_PAUSE);
    }
}

/// The Assetto Corsa UDP listener.
pub struct AssettoCorsaUdpSource {
    shared: Arc<Mutex<Shared>>,
    control: Arc<Control>,
    worker: Option<JoinHandle<()>>,
}

impl AssettoCorsaUdpSource {
    /// Starts the worker.
    pub fn new(settings: ListenerSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    /// The worker reads the current NETWORK_PORT only after creating its selected session's socket.
    pub fn with_live_network_ports(reader: NetworkPortsReader) -> Self {
        Self::start(SessionPort::Native(reader), Box::new(SystemNetwork), Parameters::for_listener())
    }

    fn with_network(settings: ListenerSettings, net: Box<dyn Network>) -> Self {
        Self::with_parts(settings, net, Parameters::for_listener())
    }

    fn with_parts(settings: ListenerSettings, net: Box<dyn Network>, parameters: Arc<Parameters>) -> Self {
        Self::start(SessionPort::Numeric(settings.port), net, parameters)
    }

    fn start(port: SessionPort, net: Box<dyn Network>, parameters: Arc<Parameters>) -> Self {
        let forwarder = Arc::new(Forwarder::with(parameters.clone(), PortForwarding::for_listener()));
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let control = Arc::new(Control::new(parameters));
        let (worker_shared, worker_control) = (shared.clone(), control.clone());
        let worker = std::thread::Builder::new()
            .name("threadacudp".into())
            .spawn(move || worker(net, port, forwarder, worker_shared, worker_control))
            .ok();
        AssettoCorsaUdpSource { shared, control, worker }
    }

    fn connected(&self) -> bool {
        self.shared.lock().expect("acudp state").connected
    }

    /// Writes the channels.
    fn write_channels(&self, t: &mut Telemetry) {
        zero_listener_channels(t);
        let Some(c) = self.shared.lock().expect("acudp state").car_info else { return };
        t.gear = f64::from(c.gear) - 1.0;
        t.speed = c.speed_ms;
        t.steering_wheel_angle = c.steer;
        t.suspension_travel_lf = c.suspension_height[0] / 3.0;
        t.suspension_travel_rf = c.suspension_height[1] / 3.0;
        t.suspension_travel_lr = c.suspension_height[2] / 3.0;
        t.suspension_travel_rr = c.suspension_height[3] / 3.0;
        t.lateral_acceleration = c.acc_g_horizontal * 8.0;
        t.longitudinal_acceleration = c.acc_g_frontal * 10.0;
        t.vertical_acceleration = c.acc_g_vertical * 2.0;
        t.rpm = c.engine_rpm;
        t.max_rpm = MAX_RPM;
    }
}

fn read_car_info(name: &str, mut current: impl FnMut() -> Option<CarInfo>) -> NativeRead {
    use LegacyValue::{Float, Int};
    if current().is_none() {
        return NativeRead::Value(Int(0));
    }
    match name {
        "max_rpm" => return NativeRead::Value(Int(MAX_RPM as i64)),
        "gear"
        | "rpm"
        | "speed"
        | "steering_wheel_angle"
        | "suspension_travel_lf"
        | "suspension_travel_rf"
        | "suspension_travel_lr"
        | "suspension_travel_rr"
        | "lateral_acceleration"
        | "longitudinal_acceleration"
        | "vertical_acceleration" => {}
        _ => return NativeRead::Value(Int(0)),
    }
    let Some(c) = current() else { return NativeRead::Raised };
    NativeRead::Value(match name {
        "gear" => Int(i64::from(c.gear) - 1),
        "rpm" => Float(c.engine_rpm),
        "speed" => Float(c.speed_ms),
        "steering_wheel_angle" => Float(c.steer),
        "suspension_travel_lf" => Float(c.suspension_height[0] / 3.0),
        "suspension_travel_rf" => Float(c.suspension_height[1] / 3.0),
        "suspension_travel_lr" => Float(c.suspension_height[2] / 3.0),
        "suspension_travel_rr" => Float(c.suspension_height[3] / 3.0),
        "lateral_acceleration" => Float(c.acc_g_horizontal * 8.0),
        "longitudinal_acceleration" => Float(c.acc_g_frontal * 10.0),
        "vertical_acceleration" => Float(c.acc_g_vertical * 2.0),
        _ => Int(0),
    })
}

impl Source for AssettoCorsaUdpSource {
    fn id(&self) -> &'static str {
        GAME_ID
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    fn read_native(&self, name: &str) -> NativeRead {
        read_car_info(name, || self.shared.lock().expect("acudp state").car_info)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.connected())
    }

    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.connected())
    }

    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        self.write_channels(telemetry);
        Ok(self.connected())
    }
}

impl Drop for AssettoCorsaUdpSource {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::support::net::fake::FakeNetwork;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn native_session_reads_current_port_only_when_selected_and_retains_state_on_setup_error() {
        use openrigcue_model::native_value::Dict;
        let current = Arc::new(Mutex::new(Dict::from_iter([("acudp".into(), LegacyValue::from("6001"))])));
        let reads = Arc::new(AtomicUsize::new(0));
        let reader: NetworkPortsReader = {
            let current = current.clone();
            let reads = reads.clone();
            Arc::new(move || {
                reads.fetch_add(1, Ordering::Relaxed);
                current.lock().unwrap().clone()
            })
        };
        let parameters = Arc::new(Parameters::default());
        let control = Control::new(parameters.clone());
        let port = SessionPort::Native(reader);
        let mut net = FakeNetwork::default();
        assert!(port.open_selected(&mut net, &control).unwrap().is_none());
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        parameters.set_game_running("acudp");
        let shared = Mutex::new(Shared { connected: true, car_info: Some(CarInfo::parse(&car_info()).unwrap()) });
        let first = port.open_selected(&mut net, &control).unwrap().unwrap();
        drop(first);
        current.lock().unwrap().insert("acudp", "6002".into());
        let mut second = port.open_selected(&mut net, &control).unwrap().unwrap();
        assert_eq!(net.bound().iter().map(|endpoint| endpoint.bind.port()).collect::<Vec<_>>(), [6001, 6002]);
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        current.lock().unwrap().insert("acudp", "bad".into());
        let error = port.open_selected(&mut net, &control).err().unwrap();
        assert_eq!(error.to_string(), "invalid literal for int() with base 10: 'bad'");
        assert_eq!(net.bound().len(), 2);
        assert_eq!(net.sent().len(), 2);
        assert!(shared.lock().unwrap().connected, "setup failure leaves previous connection untouched");
        assert!(shared.lock().unwrap().car_info.is_some());
        assert_eq!(second.step(&Forwarder::default(), &shared), Step::SessionLost);
        assert!(!shared.lock().unwrap().connected, "receive failure has the separate clearing boundary");
        assert!(shared.lock().unwrap().car_info.is_some());
    }

    #[test]
    fn native_port_conversion_keeps_wide_values_and_required_lookup_failures() {
        use openrigcue_model::native_value::Dict;
        let current = Arc::new(Mutex::new(Dict::new()));
        let reader: NetworkPortsReader = {
            let current = current.clone();
            Arc::new(move || current.lock().unwrap().clone())
        };
        assert_eq!(native_port(&reader).unwrap_err().to_string(), "'acudp'");
        for (value, expected) in [
            (LegacyValue::from("70000"), ImportedPort::OutOfRange),
            (LegacyValue::from("-1"), ImportedPort::OutOfRange),
            (LegacyValue::from("999999999999999999999999999999999999"), ImportedPort::OutOfRange),
            (LegacyValue::from("+006001"), ImportedPort::Valid(6001)),
            (LegacyValue::Bool(true), ImportedPort::Valid(1)),
            (LegacyValue::Float(-0.5), ImportedPort::Valid(0)),
            (LegacyValue::Bytes(b" 6001 ".to_vec()), ImportedPort::Valid(6001)),
        ] {
            current.lock().unwrap().insert("acudp", value);
            assert_eq!(native_port(&reader).unwrap(), expected);
        }
        current.lock().unwrap().insert("acudp", LegacyValue::None);
        assert!(native_port(&reader).unwrap_err().to_string().contains("not 'NoneType'"));
    }

    #[test]
    fn native_session_sends_handshake_before_setting_timeout() {
        use std::io;
        struct TraceSocket {
            calls: Arc<Mutex<Vec<String>>>,
            fail: &'static str,
        }
        impl Datagrams for TraceSocket {
            fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
                assert_eq!(timeout, Some(RECEIVE_TIMEOUT));
                self.calls.lock().unwrap().push("timeout".into());
                if self.fail == "timeout" {
                    Err(io::Error::other("timeout failed"))
                } else {
                    Ok(())
                }
            }
            fn receive(&mut self, _: usize) -> io::Result<(Vec<u8>, std::net::SocketAddr)> {
                panic!("no receive")
            }
            fn send_to(&mut self, data: &[u8], to: std::net::SocketAddr) -> io::Result<()> {
                assert_eq!(data, request(HANDSHAKE));
                assert_eq!(to, CONSOLE.into());
                self.calls.lock().unwrap().push("handshake".into());
                if self.fail == "send" {
                    Err(io::Error::other("send failed"))
                } else {
                    Ok(())
                }
            }
            fn try_clone(&self) -> io::Result<Box<dyn Datagrams>> {
                panic!("no clone")
            }
        }
        struct TraceNetwork {
            calls: Arc<Mutex<Vec<String>>>,
            fail: &'static str,
        }
        impl Network for TraceNetwork {
            fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
                panic!("native reader required")
            }
            fn bind_live(
                &mut self,
                endpoint: &Endpoint,
                read_port: &mut dyn FnMut() -> io::Result<ImportedPort>,
            ) -> io::Result<Box<dyn Datagrams>> {
                assert!(endpoint.reuse_address && endpoint.broadcast);
                self.calls.lock().unwrap().push(format!("bind timeout {:?}", endpoint.timeout));
                assert_eq!(read_port()?, ImportedPort::Valid(6001));
                if self.fail == "bind" {
                    return Err(io::Error::other("bind failed"));
                }
                Ok(Box::new(TraceSocket { calls: self.calls.clone(), fail: self.fail }))
            }
        }
        for fail in ["", "bind", "send", "timeout"] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let result = Session::open_native(&mut TraceNetwork { calls: calls.clone(), fail }, &mut || {
                Ok(ImportedPort::Valid(6001))
            });
            assert_eq!(result.is_err(), !fail.is_empty());
            let expected = match fail {
                "bind" => vec!["bind timeout None"],
                "send" => vec!["bind timeout None", "handshake"],
                _ => vec!["bind timeout None", "handshake", "timeout"],
            };
            assert_eq!(*calls.lock().unwrap(), expected, "{fail}");
        }
    }

    fn inactive_source() -> AssettoCorsaUdpSource {
        AssettoCorsaUdpSource {
            shared: Arc::default(),
            control: Arc::new(Control::new(Parameters::for_listener())),
            worker: None,
        }
    }

    #[test]
    fn native_getter_rereads_current_packet_after_presence_gate() {
        let first = CarInfo::parse(&car_info()).unwrap();
        let mut second = first;
        second.engine_rpm = 7200.0;
        second.gear = 5;
        second.speed_ms = 45.0;
        second.steer = 15.0;
        second.suspension_height = [0.3, 0.6, 0.9, 1.2];
        second.acc_g_horizontal = 1.0;
        second.acc_g_frontal = 2.0;
        second.acc_g_vertical = 3.0;
        for (name, expected) in [
            ("rpm", LegacyValue::Float(7200.0)),
            ("gear", LegacyValue::Int(4)),
            ("speed", LegacyValue::Float(45.0)),
            ("steering_wheel_angle", LegacyValue::Float(15.0)),
            ("suspension_travel_lf", LegacyValue::Float(0.3 / 3.0)),
            ("suspension_travel_rf", LegacyValue::Float(0.6 / 3.0)),
            ("suspension_travel_lr", LegacyValue::Float(0.9 / 3.0)),
            ("suspension_travel_rr", LegacyValue::Float(1.2 / 3.0)),
            ("lateral_acceleration", LegacyValue::Float(8.0)),
            ("longitudinal_acceleration", LegacyValue::Float(20.0)),
            ("vertical_acceleration", LegacyValue::Float(6.0)),
        ] {
            let mut reads = 0;
            let result = read_car_info(name, || {
                reads += 1;
                Some(if reads == 1 { first } else { second })
            });
            assert_eq!(result, NativeRead::Value(expected), "{name}");
            assert_eq!(reads, 2, "{name}");
        }
    }

    #[test]
    fn native_getter_preserves_presence_gate_and_constant_short_circuits() {
        let car = CarInfo::parse(&car_info()).unwrap();
        for (name, present, expected) in [
            ("rpm", false, 0),
            ("max_rpm", false, 0),
            ("unknown", false, 0),
            ("max_rpm", true, 10000),
            ("unknown", true, 0),
        ] {
            let mut reads = 0;
            assert_eq!(
                read_car_info(name, || {
                    reads += 1;
                    assert_eq!(reads, 1, "no branch field read");
                    present.then_some(car)
                }),
                NativeRead::Value(LegacyValue::Int(expected)),
                "{name}/{present}"
            );
            assert_eq!(reads, 1);
        }
        let mut reads = 0;
        assert_eq!(
            read_car_info("rpm", || {
                reads += 1;
                (reads == 1).then_some(car)
            }),
            NativeRead::Raised
        );
        assert_eq!(reads, 2);
    }

    #[test]
    fn native_output_kinds() {
        let s = inactive_source();
        for name in ["max_rpm", "gear", "speed", "car_name"] {
            assert_eq!(s.native_channel(name), Some(LegacyValue::Int(0)));
        }
        s.shared.lock().unwrap().car_info = Some(CarInfo::parse(&car_info()).unwrap());
        for (name, value) in [
            ("gear", LegacyValue::Int(2)),
            ("max_rpm", LegacyValue::Int(10000)),
            ("rpm", LegacyValue::Float(5500.0)),
            ("speed", LegacyValue::Float(33.0)),
            ("steering_wheel_angle", LegacyValue::Float(-90.0)),
            ("lateral_acceleration", LegacyValue::Float(-10.0)),
            ("car_name", LegacyValue::Int(0)),
        ] {
            assert_eq!(s.native_channel(name), Some(value), "{name}");
        }
    }

    #[test]
    fn native_adapter_reads_current_car_info_without_a_worker() {
        use crate::derive::{Listener, SourceListener};
        use crate::NativeRead;
        let s = inactive_source();
        let shared = s.shared.clone();
        shared.lock().unwrap().car_info = Some(CarInfo::parse(&car_info()).unwrap());
        let mut listener = SourceListener::new(s);
        listener.receive();
        shared.lock().unwrap().car_info.as_mut().unwrap().engine_rpm = 6000.0;
        assert_eq!(listener.read_native("rpm"), NativeRead::Value(LegacyValue::Float(6000.0)));
        assert_eq!(listener.read_native("gear"), NativeRead::Value(LegacyValue::Int(2)));
    }

    /// An RTCarInfo update in its C struct layout.
    fn car_info() -> Vec<u8> {
        let mut p = vec![0u8; CAR_INFO_LEN];
        let mut put = |o: usize, v: f32| p[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(16, 33.0);
        put(28, 0.5);
        put(32, -1.25);
        put(36, 0.3);
        put(68, 5500.0);
        put(72, -90.0);
        put(292, 0.06);
        put(304, 0.09);
        p[76..80].copy_from_slice(&3i32.to_le_bytes());
        p
    }

    #[test]
    fn handshake_subscribe_and_updates() {
        // Worker: handshake broadcast, subscribe after the 408-byte response
        let mut net = FakeNetwork::default();
        let shared = Mutex::new(Shared::default());
        let fwd = Forwarder::default();
        let mut session = Session::open(&mut net, DEFAULT_PORT).unwrap();
        let ep = &net.bound()[0];
        assert!(ep.reuse_address && ep.broadcast);
        assert_eq!(ep.bind, "0.0.0.0:9997".parse().unwrap());
        assert_eq!(net.sent()[0], (vec![1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0], "255.255.255.255:9996".parse().unwrap()));
        net.push(DEFAULT_PORT, &[0u8; 408]);
        assert_eq!(session.step(&fwd, &shared), Step::Continue);
        assert!(shared.lock().unwrap().connected);
        assert_eq!(net.sent()[1].0, vec![1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]);
        net.push(DEFAULT_PORT, &car_info());
        assert_eq!(session.step(&fwd, &shared), Step::Continue);
        assert!(shared.lock().unwrap().car_info.is_some());
        assert_eq!(session.step(&fwd, &shared), Step::SessionLost, "timeout ends the session");
        assert!(!shared.lock().unwrap().connected);
    }

    #[test]
    fn channels_from_car_info() {
        // Channel values
        let s = inactive_source();
        let mut t = Telemetry::off();
        s.write_channels(&mut t);
        assert_eq!(t.max_rpm, 0.0, "no carinfo yet: 0");
        s.shared.lock().unwrap().car_info = Some(CarInfo::parse(&car_info()).unwrap());
        s.write_channels(&mut t);
        assert_eq!((t.gear, t.speed, t.steering_wheel_angle, t.rpm, t.max_rpm), (2.0, 33.0, -90.0, 5500.0, 10000.0));
        assert_eq!(t.lateral_acceleration, -10.0);
        assert_eq!(t.longitudinal_acceleration, f64::from(0.3f32) * 10.0);
        assert_eq!(t.vertical_acceleration, 1.0);
        assert_eq!(t.suspension_travel_lf, f64::from(0.06f32) / 3.0);
        assert_eq!(t.suspension_travel_rr, f64::from(0.09f32) / 3.0);
        assert_eq!(t.flag, 0.0);
    }

    /// Waits up to 5 s: the worker looks at the selection every 2 s.
    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        (0..1000).any(|_| {
            std::thread::sleep(Duration::from_millis(5));
            cond()
        })
    }

    #[test]
    fn worker_follows_game_running() {
        // The worker opens a session only while
        // the running game is `acudp`; otherwise it sleeps 2 s.
        // Liveness tests do not matter.
        let parameters = Arc::new(Parameters::default());
        parameters.set_game_running("dirt4");
        let net = FakeNetwork::default();
        let mut s = AssettoCorsaUdpSource::with_parts(
            ListenerSettings::with_port(DEFAULT_PORT),
            Box::new(net.clone()),
            parameters.clone(),
        );
        assert!(!s.is_running().unwrap());
        std::thread::sleep(Duration::from_millis(100));
        assert!(net.bound().is_empty(), "another game runs: no session although a liveness test ran");
        net.push(DEFAULT_PORT, &[0u8; 408]);
        parameters.set_game_running("acudp");
        assert!(wait_for(|| net.sent().len() >= 2), "selected without a liveness test: handshake and subscribe");
        assert_eq!(net.bound().len(), 1);
    }

    #[test]
    fn a_failed_subscribe_ends_the_worker_and_keeps_connected() {
        // Sending the subscribe request is not covered by the receive failure handling;
        // its error ends the worker after the connected flag was set
        let mut net = FakeNetwork::default();
        let mut session = Session::open(&mut net, DEFAULT_PORT).unwrap();
        net.refuse_send(true);
        net.push(DEFAULT_PORT, &[0u8; HANDSHAKE_RESPONSE_LEN]);
        let shared = Mutex::new(Shared::default());
        assert_eq!(session.step(&Forwarder::default(), &shared), Step::WorkerEnds);
        assert!(shared.lock().unwrap().connected);
    }

    #[test]
    fn worker_dies_when_the_socket_cannot_be_opened() {
        // A failure while binding ends the worker for good
        let net = FakeNetwork::default();
        net.refuse_bind(true);
        let parameters = Arc::new(Parameters::default());
        parameters.set_game_running("acudp");
        let mut s = AssettoCorsaUdpSource::with_parts(
            ListenerSettings::with_port(DEFAULT_PORT),
            Box::new(net.clone()),
            parameters,
        );
        assert!(!s.is_running().unwrap());
        assert!(wait_for(|| s.worker.as_ref().is_some_and(JoinHandle::is_finished)));
        net.refuse_bind(false);
        net.push(DEFAULT_PORT, &[0u8; 408]);
        assert!(!s.is_running().unwrap());
        assert!(net.bound().is_empty(), "no new attempt");
    }
}

//! Farming Simulator 19 / 22 / 25: the shipped TelemetryMod writes a `{"name": value, ...}` text once per game update
//! to the message-mode pipe `\\.\pipe\FMTelemetry`, reached through the bridge's pipe relay (listener key `fs`).

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime};

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::pipe_relay::{self, RelayListener, RelayStream};
use super::python::int_from_str;
use super::python::literal::{parse_dict, Literal};
use crate::udp_flight::{clear_listener_channels, AngularRates, FlightSource};
use crate::{Source, SourceError};

/// Authored telemetry mods copied into `My Games\FarmingSimulator20xx\mods`.
/// The scripts, descriptors and artwork are built by `package-fs-mods`
/// (`game-plugins/farming-simulator`, which holds their sources and checks).
/// Each mod writes to `\\.\pipe\FMTelemetry` once per update a `{"name": %.4f, ...}` message, the pipe opened on
/// the first update and every 256 updates after closing (flush, close) the previous handle.
pub const TELEMETRY_MODS: [(&str, &[u8]); 3] = [
    ("FS19_TelemetryMod.zip", include_bytes!("farming_simulator_mod/FS19_TelemetryMod.zip")),
    ("FS22_TelemetryMod.zip", include_bytes!("farming_simulator_mod/FS22_TelemetryMod.zip")),
    ("FS25_TelemetryMod.zip", include_bytes!("farming_simulator_mod/FS25_TelemetryMod.zip")),
];

/// Name of the Windows pipe (`\\.\pipe\FMTelemetry`).
pub const PIPE_NAME: &str = "FMTelemetry";
/// Data older than this means the game stopped sending.
const FRESHNESS: Duration = Duration::from_secs(1);
/// Polling interval of the wait for new data.
const DATA_POLL: Duration = Duration::from_millis(10);
const ACCEPT_POLL: Duration = Duration::from_millis(100);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One pipe client of the server: its received messages, then an end marker
/// (`None`) once its pipe read failed.
#[derive(Debug, Default)]
struct PipeClient {
    queue: VecDeque<Option<Vec<u8>>>,
    alive: bool,
    /// The relay of an ended client, kept until the client is dropped: closing it is what makes
    /// the bridge close the client's pipe instance.
    ended_relay: Option<RelayStream>,
}

/// The client list and the "new data" flag of the pipe server.
#[derive(Debug, Default)]
struct PipeServer {
    clients: Mutex<Vec<Arc<Mutex<PipeClient>>>>,
    has_data: AtomicBool,
}

impl PipeServer {
    /// A newly connected client, after dropping clients that are dead and drained.
    fn connect(&self) -> Arc<Mutex<PipeClient>> {
        let mut clients = lock(&self.clients);
        clients.retain(|client| {
            let mut client = lock(client);
            let keep = client.alive || !client.queue.is_empty();
            if !keep {
                // The bridge closes the instance once this relay is closed.
                client.ended_relay = None;
            }
            keep
        });
        let client = Arc::new(Mutex::new(PipeClient { queue: VecDeque::new(), alive: true, ended_relay: None }));
        clients.push(Arc::clone(&client));
        client
    }

    /// One message read from `client`.
    fn deliver(&self, client: &Mutex<PipeClient>, message: Vec<u8>) {
        lock(client).queue.push_back(Some(message));
        self.has_data.store(true, Ordering::SeqCst);
    }

    /// The client's pipe read failed: an end marker is queued (without raising the data flag) and it is dead. Its
    /// relay stays open until the client is dropped.
    fn disconnect(&self, client: &Mutex<PipeClient>, relay: Option<RelayStream>) {
        let mut client = lock(client);
        client.queue.push_back(None);
        client.alive = false;
        client.ended_relay = relay;
    }

    /// Takes at most one message from every client, in connection order.
    fn take_round(&self) -> Vec<Option<Vec<u8>>> {
        let clients = lock(&self.clients).clone();
        clients.iter().filter_map(|client| lock(client).queue.pop_front()).collect()
    }

    /// Waits until a message arrived since the last wait, then clears the flag.
    /// `false` when asked to stop.
    fn wait_for_data(&self, stop: &AtomicBool) -> bool {
        while !self.has_data.swap(false, Ordering::SeqCst) {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            thread::sleep(DATA_POLL);
        }
        true
    }
}

/// The latest decoded payload and when it arrived.
#[derive(Debug, Default)]
struct Received {
    payload: HashMap<String, Literal>,
    /// Arrival time on the wall clock; the liveness test compares it with the current wall-clock time.
    at: Option<SystemTime>,
}

/// Handles one message. A client's end marker
/// or bytes that are not UTF-8 are skipped without effect. Otherwise the arrival time is
/// stamped, then the payload is replaced when the text is a literal dict; a non-literal text
/// keeps the stamp and the previous payload.
fn absorb(message: Option<Vec<u8>>, received: &Mutex<Received>) {
    let Some(text) = message.and_then(|bytes| String::from_utf8(bytes).ok()) else { return };
    lock(received).at = Some(SystemTime::now());
    if let Ok(payload) = parse_dict(&text) {
        lock(received).payload = payload;
    }
}

/// Body of the reader thread: rounds over the clients until the source is dropped.
fn run_reader(server: Arc<PipeServer>, received: Arc<Mutex<Received>>, stop: Arc<AtomicBool>) {
    *lock(&received) = Received::default();
    loop {
        for message in server.take_round() {
            absorb(message, &received);
        }
        if !server.wait_for_data(&stop) {
            return;
        }
    }
}

/// Reads relay frames of one client until its connection ends.
fn run_client(server: Arc<PipeServer>, client: Arc<Mutex<PipeClient>>, mut stream: RelayStream) {
    while let Ok(Some(message)) = pipe_relay::read_chunk(&mut stream) {
        server.deliver(&client, message);
    }
    server.disconnect(&client, Some(stream));
}

/// Body of the server thread: accepts relay connections, one reader thread per client. The listener is bound
/// before the thread starts, so the TCP route exists as soon as the source does and a bridge connection arriving
/// right after the service starts its relay is not closed as unrouted.
fn run_listener(listener: RelayListener, server: Arc<PipeServer>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok(Some(stream)) => {
                let client = server.connect();
                let server = Arc::clone(&server);
                if let Err(error) =
                    thread::Builder::new().name("readerentry".into()).spawn(move || run_client(server, client, stream))
                {
                    tracing::warn!(%error, "Farming Simulator pipe client not served");
                }
            }
            Ok(None) => thread::sleep(ACCEPT_POLL),
            Err(error) => {
                tracing::warn!(%error, "Farming Simulator pipe relay accept failed");
                thread::sleep(ACCEPT_POLL);
            }
        }
    }
}

/// The Farming Simulator listener over its pipe server, which starts with the
/// source and runs until it is dropped.
pub struct FarmingSimulator {
    received: Arc<Mutex<Received>>,
    stop: Arc<AtomicBool>,
}

impl FarmingSimulator {
    #[cfg(test)]
    pub(crate) fn with_fixture_payload(text: &str) -> Self {
        Self {
            received: Arc::new(Mutex::new(Received {
                payload: parse_dict(text).unwrap(),
                at: Some(SystemTime::now()),
            })),
            stop: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Starts the pipe server on the relay socket of [`PIPE_NAME`] below [`pipe_relay::runtime_dir`] and the
    /// reader thread.
    pub fn new() -> Self {
        Self::with_runtime_dir(&pipe_relay::runtime_dir())
    }

    /// Starts the pipe server on the relay socket of [`PIPE_NAME`] below `runtime_dir`
    /// (`<runtime_dir>/openrigcue/pipe-FMTelemetry`) and the reader thread.
    pub fn with_runtime_dir(runtime_dir: &Path) -> Self {
        Self::serving(runtime_dir, PIPE_NAME)
    }

    /// Binds the relay of `pipe_name` (registering its TCP route) and only then starts the server threads.
    fn serving(runtime_dir: &Path, pipe_name: &str) -> Self {
        let source = FarmingSimulator { received: Arc::default(), stop: Arc::new(AtomicBool::new(false)) };
        let server = Arc::new(PipeServer::default());
        let (listener_server, listener_stop) = (Arc::clone(&server), Arc::clone(&source.stop));
        let (received, reader_stop) = (Arc::clone(&source.received), Arc::clone(&source.stop));
        let listener = RelayListener::bind(runtime_dir, pipe_name);
        // The bridge serves this pipe in the game's prefix only while the application runs, which it learns from
        // the control connection of the application's own mapping objects: make sure that route exists.
        let _ = crate::shared_memory::held::Registry::global();
        let spawned = thread::Builder::new()
            .name("fs_server".into())
            .spawn(move || run_listener(listener, listener_server, listener_stop))
            .and_then(|_| {
                thread::Builder::new().name("fs_thread".into()).spawn(move || run_reader(server, received, reader_stop))
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "Farming Simulator pipe server not started");
        }
        source
    }

    /// A channel read: x, y, z, w come from `qx1..qw1` (a missing one makes every channel 0);
    /// yaw/roll/pitch are the mod's quaternion Euler conversion in degrees (pitch negated); unanswered channels 0.
    fn write_channels(payload: &HashMap<String, Literal>, t: &mut Telemetry) {
        let previous = t.clone();
        clear_listener_channels(t);
        let quaternion = ["qx1", "qy1", "qz1", "qw1"].map(|key| payload.get(key));
        let [Some(x), Some(y), Some(z), Some(w)] = quaternion else { return };
        let number = |key: &str| payload.get(key).and_then(Literal::number);
        // A value returned unchanged: numbers as they are, strings through the dispatcher's integer conversion;
        // anything else keeps the channel's previous value (the dispatcher's handling of a failed conversion).
        let raw = |key: &str, keep: f64| match payload.get(key) {
            Some(Literal::Str(text)) => int_from_str(text).map_or(keep, |i| i as f64),
            Some(value) => value.number().unwrap_or(keep),
            None => 0.0,
        };
        t.rpm = raw("rpm", previous.rpm);
        // The game's speed (km/h), not converted.
        t.speed = raw("speed", previous.speed);
        t.vertical_acceleration = number("avZ1").map_or(0.0, |av| av * 10.0);
        t.longitudinal_acceleration = raw("lvX1", previous.longitudinal_acceleration);
        t.lateral_acceleration = raw("lvZ1", previous.lateral_acceleration);
        let (Some(x), Some(y), Some(z), Some(w)) = (x.number(), y.number(), z.number(), w.number()) else { return };
        let degrees = 180.0 / std::f64::consts::PI;
        t.yaw = degrees * (2.0 * y * w - 2.0 * x * z).atan2(1.0 - 2.0 * y * y - 2.0 * z * z);
        let sine = 2.0 * x * y + 2.0 * z * w;
        // The arcsine is an error outside [-1, 1].
        t.roll = if sine.is_nan() || (-1.0..=1.0).contains(&sine) { degrees * sine.asin() } else { 0.0 };
        t.pitch = -(degrees * (2.0 * x * w - 2.0 * y * z).atan2(1.0 - 2.0 * x * x - 2.0 * z * z));
    }
}

/// The getter reloads the shared payload for every lookup, retaining the four quaternion values.
fn native_value_with(
    mut read: impl FnMut(&str) -> Option<Literal>,
    name: &str,
    mut report: impl FnMut(&str),
) -> LegacyValue {
    use LegacyValue::{Float, Int, Str};
    let mut get = |key: &str| read(key).ok_or_else(|| format!("'{key}'"));
    let mut answer = || -> Result<LegacyValue, String> {
        let x = get("qx1")?;
        let y = get("qy1")?;
        let z = get("qz1")?;
        let w = get("qw1")?;
        let value = match name {
            "rpm" => return Ok(get("rpm")?.native()),
            "speed" => return Ok(get("speed")?.native()),
            "longitudinal_acceleration" => return Ok(get("lvX1")?.native()),
            "lateral_acceleration" => return Ok(get("lvZ1")?.native()),
            "vertical_acceleration" => {
                return Ok(match get("avZ1")? {
                    Literal::Int(n) => n.checked_mul(10).map_or_else(|| Float(n as f64 * 10.0), Int),
                    Literal::Number(n) => Float(n * 10.0),
                    Literal::Bool(b) => Int(i64::from(b) * 10),
                    Literal::Str(text) => Str(text.repeat(10)),
                    Literal::None => {
                        return Err("unsupported operand type(s) for *: 'NoneType' and 'int'".into());
                    }
                });
            }
            "yaw" | "roll" | "pitch" => {
                // Validate the actual expression order before conversion, so a bad raw
                // quaternion reports the arithmetic error rather than a cast error.
                validate_quaternion_operands(name, [&x, &y, &z, &w])?;
                let (x, y, z, w) = (x.number().unwrap(), y.number().unwrap(), z.number().unwrap(), w.number().unwrap());
                let degrees = 180.0 / std::f64::consts::PI;
                match name {
                    "yaw" => degrees * (2.0 * y * w - 2.0 * x * z).atan2(1.0 - 2.0 * y * y - 2.0 * z * z),
                    "pitch" => -(degrees * (2.0 * x * w - 2.0 * y * z).atan2(1.0 - 2.0 * x * x - 2.0 * z * z)),
                    _ => {
                        let sine = 2.0 * x * y + 2.0 * z * w;
                        if !sine.is_nan() && !(-1.0..=1.0).contains(&sine) {
                            return Err("math domain error".into());
                        }
                        degrees * sine.asin()
                    }
                }
            }
            _ => return Ok(Int(0)),
        };
        Ok(Float(value))
    };
    answer().unwrap_or_else(|error| {
        report(&format!("error calculating the yaw, roll and pitch {error}"));
        Int(0)
    })
}

#[derive(Clone, Copy)]
enum OperandKind {
    Int,
    Float,
    Str,
    None,
}

impl OperandKind {
    fn from_literal(value: &Literal) -> Self {
        match value {
            Literal::Int(_) | Literal::Bool(_) => Self::Int,
            Literal::Number(_) => Self::Float,
            Literal::Str(_) => Self::Str,
            Literal::None => Self::None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Float => "float",
            Self::Str => "str",
            Self::None => "NoneType",
        }
    }

    fn operation(self, op: char, other: Self) -> Result<Self, String> {
        use OperandKind::*;
        match (op, self, other) {
            (_, Int, Int) => Ok(Int),
            (_, Int | Float, Int | Float) => Ok(Float),
            ('*', Str, Int) | ('*', Int, Str) | ('+', Str, Str) => Ok(Str),
            ('*', Str, n) | ('*', n, Str) => Err(format!("can't multiply sequence by non-int of type '{}'", n.name())),
            ('+', Str, n) => Err(format!("can only concatenate str (not \"{}\") to str", n.name())),
            _ => Err(format!("unsupported operand type(s) for {op}: '{}' and '{}'", self.name(), other.name())),
        }
    }
}

/// Only kind checks are needed here: numerical expressions below keep their existing f64 operations.
fn validate_quaternion_operands(name: &str, values: [&Literal; 4]) -> Result<(), String> {
    use OperandKind::Int;
    let [x, y, z, w] = values.map(OperandKind::from_literal);
    let twice_product = |a: OperandKind, b: OperandKind| Int.operation('*', a)?.operation('*', b);
    let argument = match name {
        "yaw" => twice_product(y, w)?.operation('-', twice_product(x, z)?)?,
        "pitch" => twice_product(x, w)?.operation('-', twice_product(y, z)?)?,
        _ => twice_product(x, y)?.operation('+', twice_product(z, w)?)?,
    };
    let denominator = if name != "roll" {
        let square = if name == "yaw" { y } else { x };
        Some(Int.operation('-', twice_product(square, square)?)?.operation('-', twice_product(z, z)?)?)
    } else {
        None
    };
    for operand in [Some(argument), denominator].into_iter().flatten() {
        if matches!(operand, OperandKind::Str | OperandKind::None) {
            return Err(format!("must be real number, not {}", operand.name()));
        }
    }
    Ok(())
}

#[cfg(test)]
fn native_value(payload: &HashMap<String, Literal>, name: &str) -> LegacyValue {
    native_value_with(|key| payload.get(key).cloned(), name, |_| {})
}

impl Default for FarmingSimulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for FarmingSimulator {
    fn id(&self) -> &'static str {
        "fs"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        // A channel read is atomic with respect to the reader thread: the payload
        // cannot be replaced in the middle of it, so every key comes from one dictionary.
        let payload = lock(&self.received).payload.clone();
        Some(native_value_with(|key| payload.get(key).cloned(), name, |line| println!("{line}")))
    }

    /// The liveness test: a message arrived within the last second.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        // More than one second since the last message is stale; a clock set back gives a negative difference, fresh.
        Ok(lock(&self.received).at.is_some_and(|at| at.elapsed().map_or(true, |age| age <= FRESHNESS)))
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let received = lock(&self.received);
        match received.payload.get("speed") {
            Some(Literal::Int(value)) => Ok(*value > 0),
            Some(Literal::Number(value)) => Ok(*value > 0.0),
            Some(Literal::Bool(value)) => Ok(*value),
            value => {
                let error = match value {
                    None => "'speed'",
                    Some(Literal::None) => "'>' not supported between instances of 'NoneType' and 'int'",
                    Some(Literal::Str(_)) => "'>' not supported between instances of 'str' and 'int'",
                    _ => unreachable!("numeric payload values handled above"),
                };
                println!("error checking the speeed {error}");
                Ok(false)
            }
        }
    }

    /// Numeric compatibility view of the latest payload; typed getters read that payload directly.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(t)?;
        let received = lock(&self.received);
        Self::write_channels(&received.payload, t);
        Ok(fresh)
    }
}

impl FlightSource for FarmingSimulator {
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

impl Drop for FarmingSimulator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = r#"{"avX1": 0.0000, "avY1": 0.0000, "avZ1": 0.5000, "lvX1": 1.2500, "lvY1": 0.0000,
        "lvZ1": -0.7500, "qx1": 0.0000, "qy1": 0.0000, "qz1": 0.0000, "qw1": 1.0000, "rpm": 1200.0000,
        "rpmReal": 1100.0000, "speed": 18.5000, }"#;

    fn payload(text: &str) -> HashMap<String, Literal> {
        parse_dict(text).unwrap()
    }

    #[test]
    fn native_getter_reloads_each_key_but_keeps_quaternion_locals() {
        let mut current = payload(PAYLOAD);
        current.insert("qx1".into(), Literal::Number(0.1));
        let mut reads = Vec::new();
        let answer = native_value_with(
            |key| {
                reads.push(key.to_owned());
                let value = current.get(key).cloned();
                if key == "qx1" {
                    current = payload("{'qx1':9,'qy1':0.2,'qz1':0.3,'qw1':0.9,'rpm':9000}");
                }
                value
            },
            "yaw",
            |error| panic!("unexpected caught error: {error}"),
        );
        let (x, y, z, w) = (0.1_f64, 0.2_f64, 0.3_f64, 0.9_f64);
        assert_eq!(
            answer,
            LegacyValue::Float(
                (180.0 / std::f64::consts::PI) * (2.0 * y * w - 2.0 * x * z).atan2(1.0 - 2.0 * y * y - 2.0 * z * z)
            )
        );
        assert_eq!(reads, ["qx1", "qy1", "qz1", "qw1"]);

        reads.clear();
        assert_eq!(
            native_value_with(
                |key| {
                    reads.push(key.to_owned());
                    if key == "qw1" {
                        current.insert("rpm".into(), Literal::Int(9100));
                    }
                    current.get(key).cloned()
                },
                "rpm",
                |error| panic!("unexpected caught error: {error}"),
            ),
            LegacyValue::Int(9100)
        );
        assert_eq!(reads, ["qx1", "qy1", "qz1", "qw1", "rpm"]);
    }

    #[test]
    fn native_getter_missing_key_reports_once_and_stops_at_that_lookup() {
        let mut reads = Vec::new();
        let mut errors = Vec::new();
        assert_eq!(
            native_value_with(
                |key| {
                    reads.push(key.to_owned());
                    (key != "qy1").then_some(Literal::Int(0))
                },
                "speed",
                |error| errors.push(error.to_owned()),
            ),
            LegacyValue::Int(0)
        );
        assert_eq!(reads, ["qx1", "qy1"]);
        assert_eq!(errors, ["error calculating the yaw, roll and pitch 'qy1'"]);
    }

    #[test]
    fn native_getter_reports_domain_and_arithmetic_failures_without_eager_validation() {
        let mut current = payload("{'qx1':1,'qy1':1,'qz1':1,'qw1':1,'avZ1':None,'speed':25}");
        for (channel, message) in [
            ("roll", "math domain error"),
            ("vertical_acceleration", "unsupported operand type(s) for *: 'NoneType' and 'int'"),
        ] {
            let mut errors = Vec::new();
            assert_eq!(
                native_value_with(|key| current.get(key).cloned(), channel, |e| errors.push(e.to_owned())),
                LegacyValue::Int(0)
            );
            assert_eq!(errors, [format!("error calculating the yaw, roll and pitch {message}")]);
        }
        current.insert("qx1".into(), Literal::Str("bad".into()));
        let mut errors = Vec::new();
        assert_eq!(
            native_value_with(|key| current.get(key).cloned(), "speed", |e| errors.push(e.to_owned())),
            LegacyValue::Int(25)
        );
        assert!(errors.is_empty(), "unused quaternion types do not prevent raw speed");
        assert_eq!(
            native_value_with(|key| current.get(key).cloned(), "roll", |e| errors.push(e.to_owned())),
            LegacyValue::Int(0)
        );
        assert_eq!(errors, ["error calculating the yaw, roll and pitch can only concatenate str (not \"int\") to str"]);
    }

    #[test]
    fn native_getter_missing_key_diagnostic_emitter() {
        let source = FarmingSimulator { received: Arc::default(), stop: Arc::new(AtomicBool::new(true)) };
        let listener = crate::derive::SourceListener::new(source);
        assert_eq!(
            crate::derive::Listener::read_native(&listener, "speed"),
            crate::NativeRead::Value(LegacyValue::Int(0))
        );
    }

    #[test]
    fn native_getter_reports_errors_through_source_adapter() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_memory::farming_simulator::tests::native_getter_missing_key_diagnostic_emitter",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.matches("error calculating the yaw, roll and pitch 'qx1'").count(), 1, "{stdout}");
    }

    #[test]
    fn native_receive_checks_speed_without_precalculating_payload_getters() {
        for (speed, running) in [
            (Literal::Int(1), true),
            (Literal::Int(0), false),
            (Literal::Number(f64::NAN), false),
            (Literal::Str("1".into()), false),
            (Literal::None, false),
        ] {
            let mut source = FarmingSimulator::with_fixture_payload(PAYLOAD);
            lock(&source.received).payload.insert("speed".into(), speed.clone());
            let before = Telemetry { rpm: 17.0, roll: 19.0, ..Telemetry::off() };
            let mut compatibility = before.clone();
            assert_eq!(source.receive_native(&mut compatibility).unwrap(), running);
            assert_eq!(compatibility, before);
            match speed {
                Literal::Number(value) if value.is_nan() => {
                    assert!(
                        matches!(source.native_channel("speed"), Some(LegacyValue::Float(value)) if value.is_nan())
                    );
                }
                _ => assert_eq!(source.native_channel("speed"), Some(speed.native())),
            }
        }
    }

    #[test]
    fn raw_none_is_returned_to_the_dispatcher_instead_of_failing_receive() {
        let mut received = Received { payload: payload(PAYLOAD), at: None };
        received.payload.insert("rpm".into(), Literal::None);
        let mut source =
            FarmingSimulator { received: Arc::new(Mutex::new(received)), stop: Arc::new(AtomicBool::new(true)) };
        assert!(source.poll(&mut Telemetry::off()).unwrap());
    }

    #[test]
    fn native_integer_payloads_and_asin_nan_are_preserved() {
        use LegacyValue::{Float, Int};
        let mut p = payload("{'qx1':0,'qy1':0,'qz1':0,'qw1':1,'rpm':1200,'speed':18,'lvX1':True,'lvZ1':-0,'avZ1':2}");
        assert_eq!(native_value(&p, "rpm"), Int(1200));
        assert_eq!(native_value(&p, "speed"), Int(18));
        assert_eq!(native_value(&p, "lateral_acceleration"), Int(0));
        assert_eq!(native_value(&p, "vertical_acceleration"), Int(20));
        p.insert("qz1".into(), Literal::Number(f64::INFINITY));
        p.insert("qw1".into(), Literal::Number(0.0));
        assert!(matches!(native_value(&p, "roll"), Float(v) if v.is_nan()), "math.asin accepts NaN");
        let mut t = Telemetry::off();
        FarmingSimulator::write_channels(&p, &mut t);
        assert!(t.roll.is_nan());
        p.insert("qw1".into(), Literal::Number(1.0));
        assert_eq!(native_value(&p, "roll"), Int(0), "infinite argument raises into the local catch");
    }

    /// The pipe server starts with the source: the TCP route exists once
    /// `FarmingSimulator` is constructed, so a bridge connecting right away is served and its message read.
    #[test]
    fn connection_right_after_construction_is_routed() {
        let relay = pipe_relay::TcpRelay::start("127.0.0.1:0".parse().unwrap()).unwrap();
        let name = format!("OpenRigCueTestFmStartup{}", std::process::id());
        let runtime = tempfile::tempdir().unwrap();
        let mut fs = FarmingSimulator::serving(runtime.path(), &name);
        assert!(pipe_relay::relay_socket_path_in(runtime.path(), &name).exists(), "Unix socket bound too");
        assert!(pipe_relay::is_routed(&name));
        let mut bridge = std::net::TcpStream::connect(relay.address()).unwrap();
        pipe_relay::write_chunk(&mut bridge, name.as_bytes()).unwrap();
        pipe_relay::write_chunk(&mut bridge, PAYLOAD.as_bytes()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !fs.is_running().unwrap() {
            assert!(std::time::Instant::now() < deadline, "message relayed to the reader");
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut t = Telemetry::off();
        fs.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 1200.0);
        drop(bridge);
        drop(fs);
        relay.stop();
    }

    /// Channel mapping for a level vehicle.
    #[test]
    fn get_channel_mapping() {
        let mut t = Telemetry::off();
        FarmingSimulator::write_channels(&payload(PAYLOAD), &mut t);
        assert_eq!((t.rpm, t.speed), (1200.0, 18.5));
        assert_eq!(t.vertical_acceleration, 5.0);
        assert_eq!((t.longitudinal_acceleration, t.lateral_acceleration), (1.25, -0.75));
        assert_eq!((t.yaw, t.roll, t.pitch), (0.0, 0.0, -0.0));
        assert_eq!(t.gear, 0.0);
    }

    /// Channels: the Euler formulas with their operand order; asin outside its domain reads 0.
    #[test]
    fn euler_formulas() {
        let (x, y, z, w) = (0.1_f64, 0.2_f64, 0.3_f64, 0.9_f64);
        let text = format!(r#"{{"qx1": {x}, "qy1": {y}, "qz1": {z}, "qw1": {w}, }}"#);
        let mut t = Telemetry::off();
        FarmingSimulator::write_channels(&payload(&text), &mut t);
        let d = 180.0 / std::f64::consts::PI;
        assert_eq!(t.yaw, d * (2.0 * y * w - 2.0 * x * z).atan2(1.0 - 2.0 * y * y - 2.0 * z * z));
        assert_eq!(t.roll, d * (2.0 * x * y + 2.0 * z * w).asin());
        assert_eq!(t.pitch, -(d * (2.0 * x * w - 2.0 * y * z).atan2(1.0 - 2.0 * x * x - 2.0 * z * z)));
        FarmingSimulator::write_channels(&payload(r#"{"qx1": 1, "qy1": 1, "qz1": 1, "qw1": 1}"#), &mut t);
        assert_eq!(t.roll, 0.0);
    }

    /// Channels: without the quaternion every channel reads 0, rpm included.
    #[test]
    fn missing_quaternion_zeroes_everything() {
        let mut t = Telemetry::off();
        t.rpm = 900.0;
        FarmingSimulator::write_channels(&payload(r#"{"rpm": 1000.0, "qx1": 0, "qy1": 0, "qz1": 0}"#), &mut t);
        assert_eq!(t.rpm, 0.0);
    }

    /// One message per client per round; a dead client's end marker is skipped
    /// and the reader goes on with the next client.
    #[test]
    fn server_rounds_and_dead_clients() {
        let server = PipeServer::default();
        let received = Mutex::new(Received::default());
        let first = server.connect();
        server.deliver(&first, PAYLOAD.as_bytes().to_vec());
        server.deliver(&first, b"{}".to_vec());
        let round = server.take_round();
        assert_eq!(round.len(), 1);
        for message in round {
            absorb(message, &received);
        }
        assert_eq!(lock(&received).payload.len(), 13);
        assert!(lock(&received).at.is_some());
        server.disconnect(&first, None);
        // A reconnecting client (the TelemetryMod reopens the pipe): the dead one still holds a message and its
        // end marker, so it is kept.
        let second = server.connect();
        server.deliver(&second, b"{}".to_vec());
        assert_eq!(lock(&server.clients).len(), 2);
        let round = server.take_round();
        assert_eq!(round.len(), 2);
        for message in round {
            absorb(message, &received);
        }
        assert!(lock(&received).payload.is_empty());
        // The old client's end marker comes in the same round as the new client's next message: the marker is
        // skipped without stamping, the new client's message still lands.
        server.deliver(&second, b"{'rpm': 700}".to_vec());
        let round = server.take_round();
        assert_eq!(round, vec![None, Some(b"{'rpm': 700}".to_vec())]);
        lock(&received).at = None;
        let mut messages = round.into_iter();
        absorb(messages.next().unwrap(), &received);
        assert!(lock(&received).at.is_none());
        absorb(messages.next().unwrap(), &received);
        assert!(lock(&received).at.is_some());
        assert_eq!(lock(&received).payload.get("rpm").and_then(Literal::number), Some(700.0));
        // Drained dead clients are dropped on the next connection.
        server.connect();
        assert_eq!(lock(&server.clients).len(), 2);
    }

    /// An ended client's pipe instance is closed only once the client is dropped:
    /// its relay connection stays open until then, and closing it is the bridge's cue to close the instance.
    #[test]
    fn ended_client_relay_closes_when_dropped() {
        use std::io::Read;
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut bridge = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (relay, _) = listener.accept().unwrap();
        bridge.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let server = PipeServer::default();
        let client = server.connect();
        server.disconnect(&client, Some(RelayStream::Tcp(relay)));
        // Not drained yet (its end marker is queued): a new client keeps it, and the relay stays open.
        server.connect();
        bridge.set_nonblocking(true).unwrap();
        let mut byte = [0u8; 1];
        assert_eq!(bridge.read(&mut byte).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        bridge.set_nonblocking(false).unwrap();
        server.take_round();
        server.connect();
        assert_eq!(bridge.read(&mut byte).unwrap(), 0, "relay closed once the drained client was dropped");
    }

    /// A non-literal message keeps the stamp taken before
    /// it and the previous payload; a non-UTF-8 one neither stamps nor changes the payload.
    #[test]
    fn bad_payloads_are_absorbed() {
        let received = Mutex::new(Received::default());
        absorb(Some(PAYLOAD.as_bytes().to_vec()), &received);
        lock(&received).at = None;
        absorb(Some(b"{\"a\": nan}".to_vec()), &received);
        assert!(lock(&received).at.is_some());
        assert_eq!(lock(&received).payload.len(), 13);
        lock(&received).at = None;
        absorb(Some(vec![0xff]), &received);
        assert!(lock(&received).at.is_none());
        assert_eq!(lock(&received).payload.len(), 13);
    }

    /// The wait for data returns once data arrived, clearing the flag; end markers do not raise it.
    #[test]
    fn wait_for_data_flag() {
        let server = PipeServer::default();
        let stop = AtomicBool::new(false);
        let client = server.connect();
        server.disconnect(&client, None);
        assert!(!server.has_data.load(Ordering::SeqCst));
        server.deliver(&client, b"{}".to_vec());
        assert!(server.wait_for_data(&stop));
        assert!(!server.has_data.load(Ordering::SeqCst));
        stop.store(true, Ordering::Relaxed);
        assert!(!server.wait_for_data(&stop));
    }

    /// Raw values the dispatcher cannot take: a `None` or non-integer string speed (the top-speed update's
    /// integer conversion), a `None` rpm or acceleration (the min/max update's comparison); numbers, booleans and integer
    /// strings pass.
    #[test]
    fn raw_values_retain_types_without_predicting_dispatcher_failures() {
        use LegacyValue::{Bool, Int, None as Null, Str};
        let with = |key: &str, value: &str| {
            payload(&PAYLOAD.replace(&format!("\"{key}\": "), &format!("\"{key}\": {value}, \"x{key}\": ")))
        };
        for (key, value, channel, expected) in [
            ("speed", "None", "speed", Null),
            ("speed", "'18.5'", "speed", Str("18.5".into())),
            ("speed", "'18'", "speed", Str("18".into())),
            ("speed", "True", "speed", Bool(true)),
            ("rpm", "None", "rpm", Null),
            ("rpm", "'abc'", "rpm", Str("abc".into())),
            ("lvZ1", "None", "lateral_acceleration", Null),
            ("avZ1", "None", "vertical_acceleration", Int(0)),
            ("avZ1", "True", "vertical_acceleration", Int(10)),
            ("avZ1", "'a'", "vertical_acceleration", Str("aaaaaaaaaa".into())),
        ] {
            let received = Received { payload: with(key, value), at: None };
            let source =
                FarmingSimulator { received: Arc::new(Mutex::new(received)), stop: Arc::new(AtomicBool::new(true)) };
            let mut listener = crate::derive::SourceListener::new(source);
            crate::derive::Listener::receive(&mut listener);
            assert_eq!(crate::derive::Listener::failure_point(&listener), None, "{key}={value}");
            assert_eq!(crate::derive::Listener::native_channel(&listener, channel), Some(expected), "{key}={value}");
        }
        let mut no_quaternion = payload(PAYLOAD);
        no_quaternion.remove("qw1");
        no_quaternion.insert("speed".into(), Literal::None);
        assert_eq!(native_value(&no_quaternion, "speed"), Int(0));
    }

    /// Authored game scripts and branding retain the listener's pipe and ZIP layout.
    #[test]
    fn telemetry_mods_keep_protocol_with_openrigcue_icons() {
        let expected = [
            ("FS19_TelemetryMod.zip", 4793, 0x9188_0198, ["icon.png", "modDesc.xml", "TelemetryMod.lua"], "51"),
            ("FS22_TelemetryMod.zip", 4793, 0xf43c_ea81, ["icon.png", "modDesc.xml", "TelemetryMod.lua"], "61"),
            ("FS25_TelemetryMod.zip", 4986, 0x0da2_bb41, ["TelemetryMod.lua", "icon.png", "modDesc.xml"], "91"),
        ];
        for ((name, bytes), (srs_name, len, crc, entries, desc_version)) in TELEMETRY_MODS.into_iter().zip(expected) {
            assert_eq!((name, bytes.len(), crc32fast::hash(bytes)), (srs_name, len, crc), "{name}");
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
            assert_eq!(archive.file_names().count(), 3);
            for (index, entry) in entries.iter().enumerate() {
                assert_eq!(archive.by_index(index).unwrap().name(), *entry, "{name}");
            }
            let mut text = String::new();
            std::io::Read::read_to_string(&mut archive.by_name("modDesc.xml").unwrap(), &mut text).unwrap();
            assert!(text.contains(&format!("<modDesc descVersion=\"{desc_version}\">")), "{name}");
            assert!(text.contains("<author>OpenRigCue contributors</author>"), "{name}");
        }
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(TELEMETRY_MODS[2].1)).unwrap();
        let mut lua = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("TelemetryMod.lua").unwrap(), &mut lua).unwrap();
        assert!(lua.contains(r#"TelemetryMod.Context.pipeName = "\\\\.\\pipe\\FMTelemetry""#));
    }

    /// The liveness test is false before any message.
    #[test]
    fn freshness() {
        let mut source = FarmingSimulator { received: Arc::default(), stop: Arc::new(AtomicBool::new(true)) };
        assert!(!source.is_running().unwrap());
        lock(&source.received).at = Some(SystemTime::now());
        assert!(source.is_running().unwrap());
        lock(&source.received).at = Some(SystemTime::now() - Duration::from_secs(2));
        assert!(!source.is_running().unwrap());
        lock(&source.received).at = Some(SystemTime::now() + Duration::from_secs(60));
        assert!(source.is_running().unwrap());
    }
}

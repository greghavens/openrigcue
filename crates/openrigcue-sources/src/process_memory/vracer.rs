//! V-Racer Hoverbike: JSON datagrams to `127.0.0.1:<port>` as configured in the game's `telemetry.json`
//! (listener key `vracer`).

use std::net::UdpSocket;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::listener_support::set_channel;
use super::python::json::{loads_bytes, Json};
use crate::udp_flight::{clear_listener_channels, AngularRates, FlightSource};
use crate::udp_racing::net::{Endpoint, Network, SystemNetwork};
use crate::udp_racing::receiver::Receiver;
use crate::udp_racing::socket::ImportedPort;
use crate::{NativeRead, Source, SourceError};

/// `NETWORK_PORT/vracer` default.
pub const DEFAULT_PORT: u16 = 8051;
/// Receive timeout.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
/// Receive buffer size; a longer datagram is an error rather than a truncation.
const MAX_DATAGRAM: usize = 2000;

/// JSON key → channel copied on every datagram, in copy order.
const FIELD_CHANNELS: [(&str, &str); 12] = [
    ("accelerationX", "lateral_acceleration"),
    ("accelerationZ", "longitudinal_acceleration"),
    ("accelerationY", "vertical_acceleration"),
    ("angularSpeedX", "lateral_velocity"),
    ("angularSpeedZ", "longitudinal_velocity"),
    ("angularSpeedY", "vertical_velocity"),
    ("rotationX", "pitch"),
    ("rotationZ", "roll"),
    ("rotationY", "yaw"),
    ("gear", "gear"),
    ("rpm", "rpm"),
    ("speed", "speed"),
];

/// Receives every raw datagram before it is decoded (UDP forwarding).
pub type DatagramForwarder = Box<dyn FnMut(&[u8]) + Send>;

/// Receives one datagram; the production implementation binds a fresh UDP socket per call.
pub trait DatagramPort: Send {
    fn receive_one(&mut self) -> std::io::Result<Vec<u8>>;
}

/// A new socket bound to `127.0.0.1:<port>` for every receive, closed afterwards.
pub struct LocalUdpPort {
    port: u16,
}

impl DatagramPort for LocalUdpPort {
    fn receive_one(&mut self) -> std::io::Result<Vec<u8>> {
        let socket = UdpSocket::bind(("127.0.0.1", self.port))?;
        socket.set_read_timeout(Some(RECEIVE_TIMEOUT))?;
        let mut buffer = vec![0; MAX_DATAGRAM + 1];
        let (received, _) = socket.recv_from(&mut buffer)?;
        if received > MAX_DATAGRAM {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "datagram longer than 2000 bytes"));
        }
        buffer.truncate(received);
        Ok(buffer)
    }
}

/// The V-Racer listener.
pub struct VRacer<P: DatagramPort = LocalUdpPort> {
    port: P,
    native_receiver: Option<Receiver>,
    datagram: Vec<u8>,
    /// Last decoded document and the channel values copied from it.
    document: Option<Json>,
    copied: Mutex<Vec<(&'static str, Json)>>,
    forward: Option<DatagramForwarder>,
    /// Whether the dispatcher asks for `roll` (the profile's `motion_effects.roll_type` is `'angle'`, its default
    /// when the key is missing) rather than `roll_rate`.
    roll_angle_mode: bool,
}

impl VRacer<LocalUdpPort> {
    pub fn new(port: u16) -> Self {
        Self::with_port(LocalUdpPort { port })
    }

    /// The configured `NETWORK_PORT/vracer` is converted to an integer once, and only the later bind
    /// checks its socket range. Numeric constructors take the port as given.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_imported_network(port, Box::new(SystemNetwork))
    }

    fn with_imported_network(port: ImportedPort, network: Box<dyn Network>) -> Self {
        let mut source = Self::new(0);
        source.native_receiver = Some(Receiver::without_forwarding_imported(
            network,
            Endpoint::loopback(0, RECEIVE_TIMEOUT),
            MAX_DATAGRAM,
            port,
        ));
        source
    }
}

impl<P: DatagramPort> VRacer<P> {
    pub fn with_port(port: P) -> Self {
        VRacer {
            port,
            native_receiver: None,
            datagram: Vec::new(),
            document: None,
            copied: Mutex::new(Vec::new()),
            forward: None,
            roll_angle_mode: true,
        }
    }

    /// The game configuration's `motion_effects.roll_type`: `true` for `'angle'` (also when the key is missing),
    /// `false` for a rate mode. Only in angle mode does the dispatcher read `roll`, whose read
    /// negates the stored roll; in rate mode it reads `roll_rate` and the stored roll keeps its sign.
    /// This selects the numeric compatibility poll; typed dispatch calls the requested getter directly.
    pub fn set_roll_angle_mode(&mut self, angle: bool) {
        self.roll_angle_mode = angle;
    }

    /// Every received datagram is handed to `forward` before decoding.
    pub fn set_forwarder(&mut self, forward: DatagramForwarder) {
        self.forward = Some(forward);
    }

    /// The receive step: decode the datagram and copy the mapped fields in table order. A
    /// datagram that is not JSON keeps the previous document; a document without one of the fields (or not an
    /// object) stops the copy there. Both failures make the receive step return false.
    fn decode(&mut self) -> bool {
        let Ok(document) = loads_bytes(&self.datagram) else { return false };
        let document = self.document.insert(document);
        let copied = self.copied.get_mut().unwrap_or_else(PoisonError::into_inner);
        for (key, channel) in FIELD_CHANNELS {
            let Some(value) = document.get(key).cloned() else { return false };
            match copied.iter_mut().find(|(name, _)| *name == channel) {
                Some(slot) => slot.1 = value,
                None => copied.push((channel, value)),
            }
        }
        true
    }

    /// A channel read: values only while the document's `onTrack` is truthy and
    /// its `paused` is the JSON literal `false`, everything else 0. A missing key
    /// reads 0. Each gated roll read stores the negated roll back, so the sign of
    /// a roll that no datagram refreshed flips on every tick: the dispatcher reads roll once per tick, in roll angle
    /// mode only (see [`VRacer::set_roll_angle_mode`]).
    fn write_channels(&mut self, t: &mut Telemetry) {
        clear_listener_channels(t);
        let Some(document) = &self.document else { return };
        let on_track = document.get("onTrack").is_some_and(Json::is_truthy);
        let paused_is_false = document.get("paused") == Some(&Json::Bool(false));
        if !(on_track && paused_is_false) {
            return;
        }
        for (channel, value) in self.copied.get_mut().unwrap_or_else(PoisonError::into_inner) {
            if *channel == "roll" {
                if !self.roll_angle_mode {
                    continue;
                }
                let Some(negated) = negated(value) else { continue };
                *value = negated;
            }
            if let Some(number) = value.channel_value() {
                set_channel(t, channel, number);
            }
        }
    }
}

/// A JSON value multiplied by -1; `None` where that is an error (null, object). Strings and arrays repeated
/// -1 times are empty.
fn negated(value: &Json) -> Option<Json> {
    Some(match value {
        Json::Bool(b) => Json::Int(-i64::from(*b)),
        Json::Int(i) => i.checked_neg().map_or(Json::Float(-(*i as f64)), Json::Int),
        Json::Float(f) => Json::Float(-f),
        Json::Str(_) => Json::Str(String::new()),
        Json::Array(_) => Json::Array(Vec::new()),
        Json::Null | Json::Object(_) => return None,
    })
}

impl<P: DatagramPort> Source for VRacer<P> {
    fn id(&self) -> &'static str {
        "vracer"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        match self.read_native(name) {
            NativeRead::Value(value) => Some(value),
            NativeRead::Raised | NativeRead::Unsupported => None,
        }
    }

    fn read_native(&self, name: &str) -> NativeRead {
        let zero = NativeRead::Value(LegacyValue::Int(0));
        let Some(document) = &self.document else { return zero };
        if !document.get("onTrack").is_some_and(Json::is_truthy) || document.get("paused") != Some(&Json::Bool(false)) {
            return zero;
        }
        let mut copied = self.copied.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((_, value)) = copied.iter_mut().find(|(channel, _)| *channel == name) else { return zero };
        if name == "roll" {
            let Some(negated) = negated(value) else { return zero };
            *value = negated;
        }
        NativeRead::Value(value.native())
    }

    /// The liveness test: one datagram within a second on a freshly bound socket.
    /// An empty datagram counts as not running.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        if let Some(receiver) = self.native_receiver.as_mut() {
            let Ok(datagram) = receiver.receive_and_keep() else { return Ok(false) };
            self.datagram = datagram;
            if let Some(forward) = self.forward.as_mut() {
                forward(&self.datagram);
            }
            receiver.close();
            return Ok(!self.datagram.is_empty());
        }
        let Ok(datagram) = self.port.receive_one() else { return Ok(false) };
        if let Some(forward) = self.forward.as_mut() {
            forward(&datagram);
        }
        self.datagram = datagram;
        Ok(!self.datagram.is_empty())
    }

    /// Receives and writes the channels. The channels are written even when the datagram
    /// does not decode: the dispatcher ignores the receive result and reads them from the previous
    /// document.
    /// Raw getter answers reach the shared dispatcher through native_channel, retaining its conversion and
    /// failure order rather than predicting a later error here.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let decoded = self.decode();
        self.write_channels(t);
        Ok(decoded)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.decode())
    }
}

impl<P: DatagramPort> FlightSource for VRacer<P> {
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udp_racing::net::Datagrams;
    use crate::udp_racing::socket::socket_creation_error;
    use std::collections::VecDeque;
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Arc;

    enum Step {
        Create,
        Bind,
        Timeout,
        Receive,
        Data(Vec<u8>),
    }

    #[derive(Default)]
    struct Trace {
        steps: VecDeque<Step>,
        calls: Vec<&'static str>,
        forwarded: Vec<Vec<u8>>,
    }

    struct NetworkScript(Arc<Mutex<Trace>>);
    struct SocketScript(Arc<Mutex<Trace>>, Step);

    impl Network for NetworkScript {
        fn bind(&mut self, _: &Endpoint) -> io::Result<Box<dyn Datagrams>> {
            panic!("native receiver must retain its imported port")
        }

        fn bind_imported(&mut self, endpoint: &Endpoint, port: ImportedPort) -> io::Result<Box<dyn Datagrams>> {
            assert_eq!(endpoint, &Endpoint::loopback(0, RECEIVE_TIMEOUT));
            let mut trace = self.0.lock().unwrap();
            trace.calls.push("create");
            let step = trace.steps.pop_front().unwrap();
            if matches!(step, Step::Create) {
                return Err(socket_creation_error(io::Error::other("create denied")));
            }
            trace.calls.push("range");
            port.for_bind()?;
            trace.calls.push("bind");
            if matches!(step, Step::Bind) {
                return Err(io::Error::other("bind denied"));
            }
            trace.calls.push("settimeout");
            if matches!(step, Step::Timeout) {
                return Err(io::Error::other("timeout denied"));
            }
            drop(trace);
            Ok(Box::new(SocketScript(self.0.clone(), step)))
        }
    }

    impl Datagrams for SocketScript {
        fn receive(&mut self, max_len: usize) -> io::Result<(Vec<u8>, SocketAddr)> {
            assert_eq!(max_len, 2000);
            self.0.lock().unwrap().calls.push("receive");
            match &self.1 {
                Step::Data(data) => Ok((data.clone(), "127.0.0.1:8051".parse().unwrap())),
                Step::Receive => Err(io::Error::new(io::ErrorKind::TimedOut, "receive timed out")),
                _ => panic!("setup failure cannot reach receive"),
            }
        }
        fn send_to(&mut self, _: &[u8], _: SocketAddr) -> io::Result<()> {
            panic!("receiver does not send")
        }
        fn try_clone(&self) -> io::Result<Box<dyn Datagrams>> {
            panic!("receiver does not clone")
        }
    }

    impl Drop for SocketScript {
        fn drop(&mut self) {
            self.0.lock().unwrap().calls.push("close");
        }
    }

    fn native_source(port: ImportedPort, steps: Vec<Step>) -> (VRacer, Arc<Mutex<Trace>>) {
        let trace = Arc::new(Mutex::new(Trace { steps: steps.into(), ..Trace::default() }));
        let mut source = VRacer::with_imported_network(port, Box::new(NetworkScript(trace.clone())));
        let forwarded = trace.clone();
        source.set_forwarder(Box::new(move |data| {
            let mut trace = forwarded.lock().unwrap();
            trace.calls.push("forward");
            trace.forwarded.push(data.to_vec());
        }));
        (source, trace)
    }

    #[test]
    fn native_vracer_probe_keeps_failed_prefix_and_forwards_before_close() {
        let full = FULL.as_bytes().to_vec();
        let (mut source, trace) = native_source(
            ImportedPort::Valid(8051),
            vec![
                Step::Data(full.clone()),
                Step::Create,
                Step::Bind,
                Step::Timeout,
                Step::Receive,
                Step::Data(b"{bad".to_vec()),
                Step::Data(Vec::new()),
                Step::Data(full.clone()),
            ],
        );
        assert!(trace.lock().unwrap().calls.is_empty(), "import does not create a socket");
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(7000)));
        assert_eq!(
            trace.lock().unwrap().calls,
            ["create", "range", "bind", "settimeout", "receive", "forward", "close"]
        );
        for _ in 0..4 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.datagram, full, "failed probe retains previous datagram");
        }
        assert_eq!(trace.lock().unwrap().forwarded.as_slice(), std::slice::from_ref(&full));
        assert!(source.is_running().unwrap());
        assert!(!source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(7000)));
        assert!(!source.is_running().unwrap(), "empty datagram is stored/forwarded, then false");
        assert!(source.datagram.is_empty());
        assert!(source.is_running().unwrap());
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(trace.lock().unwrap().forwarded, [full.clone(), b"{bad".to_vec(), Vec::new(), full]);
    }

    #[test]
    fn native_vracer_creates_before_range_and_retries_each_probe() {
        let (mut source, trace) = native_source(ImportedPort::OutOfRange, vec![Step::Create, Step::Data(Vec::new())]);
        source.datagram = b"previous".to_vec();
        assert!(!source.is_running().unwrap());
        assert!(!source.is_running().unwrap());
        assert_eq!(trace.lock().unwrap().calls, ["create", "create", "range"]);
        assert!(trace.lock().unwrap().forwarded.is_empty());
        assert_eq!(source.datagram, b"previous");
    }

    struct Script(Vec<std::io::Result<Vec<u8>>>);

    impl DatagramPort for Script {
        fn receive_one(&mut self) -> std::io::Result<Vec<u8>> {
            self.0.remove(0)
        }
    }

    const FULL: &str = r#"{"onTrack": true, "paused": false, "accelerationX": 0.1, "accelerationY": 0.2,
        "accelerationZ": 0.3, "angularSpeedX": 1, "angularSpeedY": 2, "angularSpeedZ": 3,
        "rotationX": 4.5, "rotationY": 90, "rotationZ": 12.5, "gear": 2, "rpm": 7000, "speed": 33.3}"#;

    fn copied_roll(source: &VRacer<Script>) -> Json {
        source.copied.lock().unwrap().iter().find(|(name, _)| *name == "roll").unwrap().1.clone()
    }

    #[test]
    fn native_receive_defers_roll_mutation_to_each_actual_getter() {
        use LegacyValue::{Float, Int};
        let mut source = VRacer::with_port(Script(vec![Ok(FULL.as_bytes().to_vec()), Ok(b"{broken".to_vec())]));
        source.set_roll_angle_mode(false);
        assert!(source.is_running().unwrap());
        let mut frame = Telemetry::off();
        frame.roll = 17.0;
        assert!(source.receive_native(&mut frame).unwrap());
        assert_eq!(frame.roll, 17.0, "receiveData does not write the dispatcher's output frame");
        assert_eq!(copied_roll(&source), Json::Float(12.5));
        assert_eq!(source.read_native("speed"), NativeRead::Value(Float(33.3)));
        assert_eq!(source.read_native("roll_rate"), NativeRead::Value(Int(0)));
        assert_eq!(copied_roll(&source), Json::Float(12.5), "other getters leave roll unchanged");
        assert_eq!(
            source.read_native("roll"),
            NativeRead::Value(Float(-12.5)),
            "the requested getter acts regardless of the compatibility poll mode"
        );
        assert_eq!(copied_roll(&source), Json::Float(-12.5));
        assert!(source.is_running().unwrap());
        assert!(!source.receive_native(&mut frame).unwrap());
        assert_eq!(copied_roll(&source), Json::Float(-12.5), "bad JSON leaves the last getter mutation intact");
        assert_eq!(source.read_native("roll"), NativeRead::Value(Float(12.5)));
        assert_eq!(source.read_native("roll"), NativeRead::Value(Float(-12.5)));
    }

    #[test]
    fn source_adapter_receives_without_replaying_roll_before_the_getter() {
        use crate::derive::{Listener, SourceListener};
        let mut source = VRacer::with_port(Script(vec![Ok(FULL.as_bytes().to_vec())]));
        assert!(source.is_running().unwrap());
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.native_channel("roll"), Some(LegacyValue::Float(-12.5)));
        assert_eq!(listener.native_channel("roll"), Some(LegacyValue::Float(12.5)));
    }

    /// Field mapping, roll negated, gated by onTrack/paused.
    #[test]
    fn datagram_mapping() {
        let mut s = VRacer::with_port(Script(vec![Ok(FULL.as_bytes().to_vec())]));
        let forwarded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = forwarded.clone();
        s.set_forwarder(Box::new(move |d| sink.lock().unwrap().push(d.len())));
        assert!(s.is_running().unwrap());
        assert_eq!(forwarded.lock().unwrap()[0], FULL.len());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!((t.lateral_acceleration, t.longitudinal_acceleration, t.vertical_acceleration), (0.1, 0.3, 0.2));
        assert_eq!((t.lateral_velocity, t.longitudinal_velocity, t.vertical_velocity), (1.0, 3.0, 2.0));
        assert_eq!((t.pitch, t.roll, t.yaw), (4.5, -12.5, 90.0));
        assert_eq!((t.gear, t.rpm, t.speed), (2.0, 7000.0, 33.3));
    }

    /// The gate: `onTrack` by truthiness, `paused`
    /// only when it is the literal `false`; a missing key reads 0.
    #[test]
    fn on_track_gate() {
        let rpm_for = |document: String| {
            let mut s = VRacer::with_port(Script(vec![Ok(document.into_bytes())]));
            let mut t = Telemetry::off();
            s.is_running().unwrap();
            s.poll(&mut t).unwrap();
            t.rpm
        };
        let with = |on_track: &str, paused: &str| {
            FULL.replace("\"onTrack\": true", &format!("\"onTrack\": {on_track}"))
                .replace("\"paused\": false", &format!("\"paused\": {paused}"))
        };
        assert_eq!(rpm_for(with("true", "true")), 0.0);
        // paused must be the literal false: 0 and 0.0 do not count.
        assert_eq!(rpm_for(with("true", "0")), 0.0);
        assert_eq!(rpm_for(with("true", "0.0")), 0.0);
        assert_eq!(rpm_for(with("true", "null")), 0.0);
        // onTrack only has to be truthy.
        for on_track in ["1", "2", "\"yes\"", "[0]", "0.5"] {
            assert_eq!(rpm_for(with(on_track, "false")), 7000.0, "{on_track}");
        }
        for on_track in ["false", "0", "0.0", "\"\"", "[]", "{}", "null"] {
            assert_eq!(rpm_for(with(on_track, "false")), 0.0, "{on_track}");
        }
        assert_eq!(rpm_for(FULL.replace("\"paused\": false,", "")), 0.0);
    }

    /// Liveness is false on timeout; the receive step is false on bad JSON with the old values kept.
    #[test]
    fn timeout_and_bad_json() {
        let mut s = VRacer::with_port(Script(vec![
            Err(std::io::ErrorKind::WouldBlock.into()),
            Ok(FULL.as_bytes().to_vec()),
            Ok(b"{broken".to_vec()),
        ]));
        assert!(!s.is_running().unwrap());
        let mut t = Telemetry::off();
        s.is_running().unwrap();
        s.poll(&mut t).unwrap();
        s.is_running().unwrap();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.rpm, 7000.0);
    }

    /// A document missing a mapped key (or not an object) makes
    /// the receive step return false; the keys before it are copied.
    #[test]
    fn missing_key_returns_false() {
        let partial = br#"{"onTrack": true, "paused": false, "accelerationX": 0.7}"#.to_vec();
        let mut s = VRacer::with_port(Script(vec![Ok(partial), Ok(b"[1, 2]".to_vec())]));
        let mut t = Telemetry::off();
        s.is_running().unwrap();
        assert!(!s.poll(&mut t).unwrap());
        assert_eq!(t.lateral_acceleration, 0.7);
        s.is_running().unwrap();
        assert!(!s.poll(&mut t).unwrap());
        // Looking up `onTrack` on an array fails, so the channel reads 0.
        assert_eq!(t.lateral_acceleration, 0.0);
    }

    /// An empty datagram counts as not running, but it is still
    /// forwarded and stored.
    #[test]
    fn empty_datagram_is_not_running() {
        let mut s = VRacer::with_port(Script(vec![Ok(Vec::new())]));
        let forwarded = std::sync::Arc::new(std::sync::Mutex::new(0));
        let sink = forwarded.clone();
        s.set_forwarder(Box::new(move |_| *sink.lock().unwrap() += 1));
        s.datagram = b"old".to_vec();
        assert!(!s.is_running().unwrap());
        assert_eq!(*forwarded.lock().unwrap(), 1);
        assert!(s.datagram.is_empty());
    }

    /// Raw values reach the typed dispatcher contract; the receive step does not predict later conversion failures.
    #[test]
    fn raw_values_reach_the_dispatcher_without_receive_failure() {
        use LegacyValue::{Bool, Dict, Int, List, None as Null, Str};
        for (field, value, channel, expected) in [
            ("speed", "null", "speed", Null),
            ("speed", "\"12.5\"", "speed", Str("12.5".into())),
            ("rpm", "null", "rpm", Null),
            ("rpm", "\"abc\"", "rpm", Str("abc".into())),
            ("gear", "true", "gear", Bool(true)),
            ("rotationX", "\"1.5\"", "pitch", Str("1.5".into())),
            ("rotationZ", "[1]", "roll", List(vec![])),
            ("rotationZ", "\"x\"", "roll", Str(String::new())),
            ("rotationZ", "null", "roll", Int(0)),
            ("rotationZ", "{}", "roll", Int(0)),
            ("accelerationX", "[1,null,true]", "lateral_acceleration", List(vec![Int(1), Null, Bool(true)])),
            ("accelerationY", "{}", "vertical_acceleration", Dict(Default::default())),
        ] {
            let document = FULL.replacen(&format!("\"{field}\": "), &format!("\"{field}\": {value}, \"x\": "), 1);
            let mut v = VRacer::with_port(Script(vec![Ok(document.into_bytes())]));
            v.is_running().unwrap();
            let mut listener = crate::derive::SourceListener::new(v);
            crate::derive::Listener::receive(&mut listener);
            assert_eq!(crate::derive::Listener::failure_point(&listener), None, "{field}={value}");
            assert_eq!(crate::derive::Listener::native_channel(&listener, channel), Some(expected), "{field}={value}");
        }
    }

    /// Rate mode: the dispatcher never reads `roll`, so the stored roll is never negated (the roll
    /// channel is not handed over either; the dispatcher reads roll_rate).
    #[test]
    fn rate_mode_leaves_roll_alone() {
        let mut v = VRacer::with_port(Script(vec![Ok(FULL.as_bytes().to_vec())]));
        v.set_roll_angle_mode(false);
        assert!(v.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(v.poll(&mut t).unwrap());
        assert!(v.poll(&mut t).unwrap());
        v.set_roll_angle_mode(true);
        let mut t = Telemetry::off();
        v.poll(&mut t).unwrap();
        assert_eq!(t.roll, -12.5, "the stored roll kept its sign while in rate mode");
    }

    /// A roll read stores the negated roll back: without a fresh rotationZ the sign flips
    /// on every gated read.
    #[test]
    fn stale_roll_flips_sign_each_tick() {
        let mut s = VRacer::with_port(Script(vec![
            Ok(FULL.as_bytes().to_vec()),
            Ok(b"{broken".to_vec()),
            Ok(b"{broken".to_vec()),
            Ok(FULL.as_bytes().to_vec()),
        ]));
        let mut t = Telemetry::off();
        let mut roll = || {
            s.is_running().unwrap();
            s.poll(&mut t).unwrap();
            t.roll
        };
        assert_eq!([roll(), roll(), roll(), roll()], [-12.5, 12.5, -12.5, -12.5]);
    }

    /// Multiplication by -1 on the JSON value kinds.
    #[test]
    fn negation_follows_python_multiplication() {
        assert_eq!(negated(&Json::Bool(true)), Some(Json::Int(-1)));
        assert_eq!(negated(&Json::Int(4)), Some(Json::Int(-4)));
        assert_eq!(negated(&Json::Float(1.5)), Some(Json::Float(-1.5)));
        assert_eq!(negated(&Json::Str("12".into())), Some(Json::Str(String::new())));
        assert_eq!(negated(&Json::Null), None);
    }
}

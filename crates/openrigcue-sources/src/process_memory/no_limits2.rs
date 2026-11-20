//! NoLimits 2 Roller Coaster: the game's telemetry server (started with `--telemetry --telemetryport=<port>`),
//! one request/response connection per poll (listener key `nl2`).

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::listener_support::Quaternion;
use crate::udp_flight::python::{fold_roll, truncate};
use crate::udp_flight::{clear_listener_channels, AngularRates, FlightSource};
use crate::udp_racing::socket::ImportedPort;
use crate::{Source, SourceError};
use socket2::{Domain, Protocol, Socket, Type};

/// `NETWORK_PORT/nl2` default.
pub const DEFAULT_PORT: u16 = 15151;

/// "Get telemetry" request: `'N'`, type 5, request id 43, no data, `'L'` (network byte order).
const TELEMETRY_REQUEST: [u8; 10] = [b'N', 0, 5, 0, 0, 0, 43, 0, 0, b'L'];
/// Bytes received and decoded: the header, eight integers and eleven floats (the reply's trailing `'L'` is not read).
const TELEMETRY_REPLY_LEN: usize = 85;

/// The telemetry record (header and the fields the listener uses).
#[derive(Debug, Clone, Copy, PartialEq)]
struct CoasterFrame {
    state: i32,
    speed: f64,
    rotation: Quaternion,
    gforce: [f64; 3],
}

impl CoasterFrame {
    /// Decodes exactly 85 bytes; any other length is rejected.
    fn parse(reply: &[u8]) -> Option<Self> {
        if reply.len() != TELEMETRY_REPLY_LEN {
            return None;
        }
        let float = |index: usize| {
            let at = 9 + 32 + 4 * index;
            f64::from(f32::from_be_bytes([reply[at], reply[at + 1], reply[at + 2], reply[at + 3]]))
        };
        Some(CoasterFrame {
            state: i32::from_be_bytes([reply[9], reply[10], reply[11], reply[12]]),
            speed: float(0),
            rotation: Quaternion { x: float(4), y: float(5), z: float(6), w: float(7) },
            gforce: [float(8), float(9), float(10)],
        })
    }
}

/// Connects to the telemetry server (the production implementation is plain TCP to 127.0.0.1).
pub trait CoasterLink: Send {
    /// Sends the request on a new connection and returns what one `recv(85)` delivered.
    fn exchange(&mut self, request: &[u8]) -> std::io::Result<Vec<u8>>;
}

/// TCP to `127.0.0.1:<port>`, blocking without timeout.
pub struct TcpCoasterLink {
    port: u16,
    imported_port: Option<ImportedPort>,
}

impl CoasterLink for TcpCoasterLink {
    fn exchange(&mut self, request: &[u8]) -> std::io::Result<Vec<u8>> {
        if let Some(port) = self.imported_port {
            return exchange_native(port, request, || Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)));
        }
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        // One write, whatever it accepts.
        let _ = stream.write(request)?;
        let mut reply = vec![0; TELEMETRY_REPLY_LEN];
        let received = stream.read(&mut reply)?;
        reply.truncate(received);
        Ok(reply)
    }
}

/// Separates socket allocation from the connect argument conversion.
trait CoasterSocket: Read + Write {
    fn connect_port(&self, port: u16) -> io::Result<()>;
}

impl CoasterSocket for Socket {
    fn connect_port(&self, port: u16) -> io::Result<()> {
        self.connect(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into())
    }
}

fn exchange_native<S: CoasterSocket>(
    port: ImportedPort,
    request: &[u8],
    create: impl FnOnce() -> io::Result<S>,
) -> io::Result<Vec<u8>> {
    let mut socket = create()?;
    let port = match port {
        ImportedPort::Valid(port) => port,
        ImportedPort::OutOfRange => return Err(io::ErrorKind::InvalidInput.into()),
    };
    socket.connect_port(port)?;
    // A single send and recv, without completing a short transfer.
    let _ = socket.write(request)?;
    let mut reply = vec![0; TELEMETRY_REPLY_LEN];
    let received = socket.read(&mut reply)?;
    reply.truncate(received);
    // The socket is closed before decoding and publication. Close errors are not represented here.
    drop(socket);
    Ok(reply)
}

/// The NoLimits 2 listener.
pub struct NoLimits2<L: CoasterLink = TcpCoasterLink> {
    link: L,
    /// The values the last decoded record left (all 0.0 before the first one).
    latest: CoasterFrame,
}

impl NoLimits2<TcpCoasterLink> {
    pub fn new(port: u16) -> Self {
        Self::with_link(TcpCoasterLink { port, imported_port: None })
    }

    /// Keep the configured port as an integer until connect checks its socket range.
    pub fn from_imported_port(port: ImportedPort) -> Self {
        Self::with_link(TcpCoasterLink { port: 0, imported_port: Some(port) })
    }
}

impl<L: CoasterLink> NoLimits2<L> {
    pub fn with_link(link: L) -> Self {
        let level = Quaternion { x: 0.0, y: 0.0, z: 0.0, w: 0.0 };
        NoLimits2 { link, latest: CoasterFrame { state: 0, speed: 0.0, rotation: level, gforce: [0.0; 3] } }
    }
}

impl<L: CoasterLink> Source for NoLimits2<L> {
    fn id(&self) -> &'static str {
        "nl2"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        let f = &self.latest;
        let value = match name {
            "speed" => return crate::udp_racing::int_text(f.speed).map(LegacyValue::Str),
            "vertical_acceleration" => f.gforce[1] - 1.0,
            "longitudinal_acceleration" => f.gforce[2] * 100.0,
            "lateral_acceleration" => -(f.gforce[0] * 100.0),
            "yaw" => -f.rotation.yaw_deg(),
            "roll" => fold_roll(-f.rotation.roll_deg()),
            "pitch" => fold_roll(f.rotation.pitch_deg()),
            _ => return Some(LegacyValue::Int(0)),
        };
        Some(LegacyValue::Float(value))
    }

    fn channel_read_raised(&self, name: &str) -> bool {
        self.native_channel(name).is_none()
    }

    /// The liveness test: a new connection, one request, one 85-byte reply; running while
    /// the reply's state word is non-zero. Any failure is "not running".
    fn is_running(&mut self) -> Result<bool, SourceError> {
        let Some(frame) = self.link.exchange(&TELEMETRY_REQUEST).ok().and_then(|reply| CoasterFrame::parse(&reply))
        else {
            return Ok(false);
        };
        self.latest = frame;
        Ok(frame.state != 0)
    }

    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(true)
    }

    /// Numeric compatibility view of the last reply; other channels read 0.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let frame = self.latest;
        let previous_speed = t.speed;
        clear_listener_channels(t);
        // The speed as an integer. NaN and infinity have no integer value,
        // and the dispatcher then keeps the previous speed.
        t.speed = truncate(frame.speed).unwrap_or(previous_speed);
        t.vertical_acceleration = frame.gforce[1] - 1.0;
        t.longitudinal_acceleration = frame.gforce[2] * 100.0;
        t.lateral_acceleration = -(frame.gforce[0] * 100.0);
        t.yaw = -frame.rotation.yaw_deg();
        t.roll = fold_roll(-frame.rotation.roll_deg());
        t.pitch = fold_roll(frame.rotation.pitch_deg());
        Ok(true)
    }
}

impl<L: CoasterLink> FlightSource for NoLimits2<L> {
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    enum Step {
        Create,
        Connect,
        Write,
        Read,
        Data { bytes: Vec<u8>, written: usize },
    }

    struct Trace {
        steps: VecDeque<Step>,
        calls: Vec<&'static str>,
        sent: Vec<Vec<u8>>,
    }

    struct NativeLink {
        port: ImportedPort,
        trace: Arc<Mutex<Trace>>,
    }

    struct SocketScript {
        step: Step,
        trace: Arc<Mutex<Trace>>,
    }

    impl CoasterSocket for SocketScript {
        fn connect_port(&self, port: u16) -> io::Result<()> {
            assert_eq!(port, 15151);
            self.trace.lock().unwrap().calls.push("connect");
            if matches!(self.step, Step::Connect) {
                Err(io::ErrorKind::ConnectionRefused.into())
            } else {
                Ok(())
            }
        }
    }

    impl Read for SocketScript {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            assert_eq!(bytes.len(), 85);
            self.trace.lock().unwrap().calls.push("recv85");
            if let Step::Data { bytes: data, .. } = &self.step {
                bytes[..data.len()].copy_from_slice(data);
                Ok(data.len())
            } else {
                assert!(matches!(self.step, Step::Read));
                Err(io::ErrorKind::ConnectionReset.into())
            }
        }
    }

    impl Write for SocketScript {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut trace = self.trace.lock().unwrap();
            trace.calls.push("send");
            trace.sent.push(bytes.to_vec());
            match self.step {
                Step::Write => Err(io::ErrorKind::BrokenPipe.into()),
                Step::Data { written, .. } => Ok(written),
                Step::Read => Ok(bytes.len()),
                _ => panic!("failed connect cannot reach send"),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("socket.send does not flush")
        }
    }

    impl Drop for SocketScript {
        fn drop(&mut self) {
            self.trace.lock().unwrap().calls.push("close");
        }
    }

    impl CoasterLink for NativeLink {
        fn exchange(&mut self, request: &[u8]) -> io::Result<Vec<u8>> {
            exchange_native(self.port, request, || {
                let mut trace = self.trace.lock().unwrap();
                trace.calls.push("create");
                let step = trace.steps.pop_front().unwrap();
                if matches!(step, Step::Create) {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                drop(trace);
                Ok(SocketScript { step, trace: self.trace.clone() })
            })
        }
    }

    fn native_source(port: ImportedPort, steps: Vec<Step>) -> (NoLimits2<NativeLink>, Arc<Mutex<Trace>>) {
        let trace = Arc::new(Mutex::new(Trace { steps: steps.into(), calls: Vec::new(), sent: Vec::new() }));
        (NoLimits2::with_link(NativeLink { port, trace: trace.clone() }), trace)
    }

    #[test]
    fn native_tcp_failures_preserve_last_frame_and_close_before_publication() {
        let mut values = [0.0; 11];
        values[0] = 12.75;
        let mut next = values;
        next[0] = 27.5;
        let (mut source, trace) = native_source(
            ImportedPort::Valid(15151),
            vec![
                Step::Data { bytes: reply(1, values), written: 0 },
                Step::Create,
                Step::Connect,
                Step::Write,
                Step::Read,
                Step::Data { bytes: reply(1, next)[..84].to_vec(), written: 10 },
                Step::Data { bytes: reply(0, next), written: 10 },
                Step::Data { bytes: reply(1, next), written: 10 },
            ],
        );
        assert!(trace.lock().unwrap().calls.is_empty());
        assert!(source.is_running().unwrap(), "zero-byte send still proceeds to the one recv");
        assert_eq!(trace.lock().unwrap().calls, ["create", "connect", "send", "recv85", "close"]);
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("12".into())));
        for _ in 0..5 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("12".into())));
        }
        assert!(!source.is_running().unwrap(), "state zero still publishes the decoded frame");
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("27".into())));
        assert!(source.is_running().unwrap());
        let trace = trace.lock().unwrap();
        assert_eq!(
            trace.calls,
            [
                "create", "connect", "send", "recv85", "close", "create", "create", "connect", "close", "create",
                "connect", "send", "close", "create", "connect", "send", "recv85", "close", "create", "connect",
                "send", "recv85", "close", "create", "connect", "send", "recv85", "close", "create", "connect", "send",
                "recv85", "close",
            ]
        );
        assert_eq!(trace.sent, vec![TELEMETRY_REQUEST.to_vec(); 6]);
    }

    #[test]
    fn native_tcp_range_is_deferred_until_after_creation_on_each_check() {
        let (mut source, trace) = native_source(
            ImportedPort::OutOfRange,
            vec![Step::Create, Step::Data { bytes: reply(1, [0.0; 11]), written: 10 }],
        );
        for _ in 0..2 {
            assert!(!source.is_running().unwrap());
            assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("0".into())));
        }
        assert_eq!(trace.lock().unwrap().calls, ["create", "create", "close"]);
        assert!(trace.lock().unwrap().sent.is_empty());
    }

    struct FakeLink {
        reply: std::io::Result<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    impl CoasterLink for FakeLink {
        fn exchange(&mut self, request: &[u8]) -> std::io::Result<Vec<u8>> {
            self.sent.push(request.to_vec());
            match &self.reply {
                Ok(bytes) => Ok(bytes.clone()),
                Err(error) => Err(std::io::Error::new(error.kind(), "fake")),
            }
        }
    }

    /// A reply: the header, eight integers and eleven floats, in network byte order.
    fn reply(state: i32, floats: [f32; 11]) -> Vec<u8> {
        let mut bytes = vec![b'S'];
        bytes.extend_from_slice(&6u16.to_be_bytes());
        bytes.extend_from_slice(&43u32.to_be_bytes());
        bytes.extend_from_slice(&76u16.to_be_bytes());
        bytes.extend_from_slice(&state.to_be_bytes());
        bytes.extend_from_slice(&[0; 28]);
        for f in floats {
            bytes.extend_from_slice(&f.to_be_bytes());
        }
        bytes
    }

    /// Liveness: request bytes, state word, short replies and refused connections.
    #[test]
    fn check_status_protocol() {
        let mut s = NoLimits2::with_link(FakeLink { reply: Ok(reply(1, [0.0; 11])), sent: vec![] });
        assert!(s.is_running().unwrap());
        assert_eq!(s.link.sent[0], [0x4E, 0x00, 0x05, 0x00, 0x00, 0x00, 0x2B, 0x00, 0x00, 0x4C]);
        s.link.reply = Ok(reply(0, [0.0; 11]));
        assert!(!s.is_running().unwrap());
        s.link.reply = Ok(reply(3, [0.0; 11])[..84].to_vec());
        assert!(!s.is_running().unwrap());
        s.link.reply = Err(std::io::ErrorKind::ConnectionRefused.into());
        assert!(!s.is_running().unwrap());
    }

    /// Channels: integer speed, heave g-1, surge ×100, sway ×-100, quaternion angles.
    #[test]
    fn get_channel_mapping() {
        let floats = [12.75, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.5, 2.0, -0.25];
        let mut s = NoLimits2::with_link(FakeLink { reply: Ok(reply(1, floats)), sent: vec![] });
        assert!(s.is_running().unwrap());
        let mut t = Telemetry::off();
        assert!(s.poll(&mut t).unwrap());
        assert_eq!(t.speed, 12.0);
        assert_eq!(t.vertical_acceleration, 1.0);
        assert_eq!(t.longitudinal_acceleration, -25.0);
        assert_eq!(t.lateral_acceleration, -50.0);
        assert_eq!((t.yaw, t.roll, t.pitch), (-0.0, -0.0, 0.0));
        assert_eq!(t.gear, 0.0);
    }

    #[test]
    fn native_text_speed_float_measurements_and_integer_defaults() {
        use LegacyValue::{Float, Int, Str};
        let mut s = NoLimits2::with_link(FakeLink { reply: Ok(reply(1, [0.0; 11])), sent: vec![] });
        assert_eq!(s.native_channel("speed"), Some(Str("0".into())));
        assert_eq!(s.native_channel("vertical_acceleration"), Some(Float(-1.0)));
        assert_eq!(s.native_channel("unknown"), Some(Int(0)));
        s.latest.speed = -12.75;
        assert_eq!(s.native_channel("speed"), Some(Str("-12".into())));
        for speed in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            s.latest.speed = speed;
            assert!(s.channel_read_raised("speed"));
            assert_eq!(s.native_channel("speed"), None);
        }
        let mut listener = crate::derive::SourceListener::new(s);
        crate::derive::Listener::receive(&mut listener);
        assert_eq!(crate::derive::Listener::channel(&listener, "speed"), None);
    }

    /// Speed: NaN or infinity has no integer value, and the dispatcher
    /// keeps the previous speed.
    #[test]
    fn non_finite_speed_keeps_previous_speed() {
        for speed in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut floats = [0.0; 11];
            floats[0] = speed;
            let mut s = NoLimits2::with_link(FakeLink { reply: Ok(reply(1, floats)), sent: vec![] });
            assert!(s.is_running().unwrap());
            let mut t = Telemetry::off();
            t.speed = 37.0;
            s.poll(&mut t).unwrap();
            assert_eq!(t.speed, 37.0, "{speed}");
        }
    }
}

//! "DynamicCab" motion input: a client speaking a small binary protocol on the named pipe
//! `\\.\pipe\DynamicCabPipe`, reached through the bridge's pipe relay (listener key `ccd`). No game in the
//! catalog selects this listener; it is registered and its pipe server runs from start-up.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::listener_support::Quaternion;
use super::pipe_relay;
use crate::shared_memory::block_reader::Fields;
use crate::udp_flight::{clear_listener_channels, AngularRates, FlightSource};
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// Name of the Windows pipe (`\\.\pipe\DynamicCabPipe`).
pub const PIPE_NAME: &str = "DynamicCabPipe";

/// Reply to the handshake packet: 8 bytes in C struct layout, padding included.
const HANDSHAKE_REPLY: [u8; 8] = [6, 0, 0, 0, 0, 0, 0, 0];
/// Reply to every other packet.
const FRAME_REPLY: [u8; 4] = [4, 0, 1, 0];
/// Packet type of the handshake (third byte of the header).
const HANDSHAKE_TYPE: i8 = 1;
/// Header length and the end of the frame body (one double and 24 floats after the header).
const HEADER_LEN: usize = 3;
const FRAME_END: usize = 107;
/// Pause after a client disconnects before accepting the next one.
const RECONNECT_PAUSE: Duration = Duration::from_secs(2);
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// The part of a per-frame packet the listener uses.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CabFrame {
    orientation: Quaternion,
    /// The car's linear velocity (x, y, z).
    velocity: [f64; 3],
}

impl CabFrame {
    /// Decodes the frame body after the header; `None` when the packet is shorter (the caller prints
    /// "frame error" and keeps the previous frame).
    fn parse(packet: &[u8]) -> Option<Self> {
        let body = packet.get(HEADER_LEN..FRAME_END)?;
        let float = |index: usize| Fields(body).f32(8 + 4 * index);
        Some(CabFrame {
            orientation: Quaternion { x: float(6), y: float(7), z: float(8), w: float(9) },
            velocity: [float(10), float(11), float(12)],
        })
    }
}

/// State shared by the pipe server and the listener (the running flag and the latest frame).
#[derive(Debug, Default)]
struct CabLink {
    running: bool,
    frame: Option<CabFrame>,
}

/// How a client session ended.
#[derive(Debug, PartialEq, Eq)]
enum SessionEnd {
    /// Read or write failure, or a packet that did not leave the session running.
    Disconnected,
    /// A packet shorter than its 3-byte header: the server thread ends on it.
    ServerStopped,
}

/// Serves one connected client: answer the handshake (which marks the link
/// running) and every frame; a frame received while not running ends the session after its reply.
fn serve_client(stream: &mut (impl Read + Write), link: &Mutex<CabLink>) -> SessionEnd {
    let lock = || link.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        let packet = match pipe_relay::read_chunk(stream) {
            Ok(Some(packet)) => packet,
            Ok(None) | Err(_) => {
                // A failed read is printed, clears the running flag and ends the session.
                crate::printed::line("error reading 1");
                lock().running = false;
                return SessionEnd::Disconnected;
            }
        };
        let Some(&kind) = packet.get(HEADER_LEN - 1) else {
            tracing::error!("DynamicCab packet shorter than its header; pipe server stops as in the original");
            return SessionEnd::ServerStopped;
        };
        let running = if kind as i8 == HANDSHAKE_TYPE {
            let written = pipe_relay::write_chunk(stream, &HANDSHAKE_REPLY).is_ok();
            if !written {
                crate::printed::line("error writing 1");
            }
            written
        } else {
            match CabFrame::parse(&packet) {
                Some(frame) => lock().frame = Some(frame),
                // A frame body that is too short is printed as a frame error.
                None => crate::printed::line(format!(
                    "frame error unpack requires a buffer of {} bytes {}",
                    FRAME_END - HEADER_LEN,
                    packet.len() - HEADER_LEN
                )),
            }
            let written = pipe_relay::write_chunk(stream, &FRAME_REPLY).is_ok();
            if !written {
                crate::printed::line("error writing 2");
            }
            written && lock().running
        };
        lock().running = running;
        if !running {
            return SessionEnd::Disconnected;
        }
    }
}

/// Body of the server thread: one client at a time (the pipe has a single instance), 2 s pause between clients.
/// The listener is bound before the thread starts, so the TCP route exists as soon as the source does and a
/// bridge connection arriving right after the service starts its relay is not closed as unrouted.
fn run_server(listener: pipe_relay::RelayListener, link: Arc<Mutex<CabLink>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let mut stream = match listener.accept() {
            Ok(Some(stream)) => stream,
            Ok(None) => {
                thread::sleep(ACCEPT_POLL);
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "DynamicCab pipe relay accept failed");
                thread::sleep(ACCEPT_POLL);
                continue;
            }
        };
        if serve_client(&mut stream, &link) == SessionEnd::ServerStopped {
            return;
        }
        // Print the disconnect notice, then pause before accepting the next client.
        crate::printed::line("trying to disconnect");
        thread::sleep(RECONNECT_PAUSE);
    }
}

/// The DynamicCab listener over its pipe server, which starts
/// with the source and runs until it is dropped.
pub struct DynamicCab {
    link: Arc<Mutex<CabLink>>,
    stop: Arc<AtomicBool>,
    /// Only the receive step updates this channel map; a newer server frame is not yet published.
    channels: HashMap<&'static str, f64>,
}

impl DynamicCab {
    /// Starts the pipe server on the relay socket of [`PIPE_NAME`] below [`pipe_relay::runtime_dir`].
    pub fn new() -> Self {
        Self::with_runtime_dir(&pipe_relay::runtime_dir())
    }

    /// Starts the pipe server on the relay socket of [`PIPE_NAME`] below `runtime_dir`
    /// (`<runtime_dir>/openrigcue/pipe-DynamicCabPipe`).
    pub fn with_runtime_dir(runtime_dir: &Path) -> Self {
        Self::serving(runtime_dir, PIPE_NAME)
    }

    /// Binds the relay of `pipe_name` (registering its TCP route) and only then starts the server thread.
    fn serving(runtime_dir: &Path, pipe_name: &str) -> Self {
        let source = Self::detached();
        let (link, stop) = (Arc::clone(&source.link), Arc::clone(&source.stop));
        let listener = pipe_relay::RelayListener::bind(runtime_dir, pipe_name);
        // The bridge serves this pipe in the game's prefix only while the application runs, which it learns from
        // the control connection of the application's own mapping objects: make sure that route exists.
        let _ = crate::shared_memory::held::Registry::global();
        if let Err(error) =
            thread::Builder::new().name("ccd_main_loop".into()).spawn(move || run_server(listener, link, stop))
        {
            tracing::warn!(%error, "DynamicCab pipe server not started");
        }
        source
    }

    /// A source without a server (tests feed the link directly).
    fn detached() -> Self {
        DynamicCab { link: Arc::default(), stop: Arc::new(AtomicBool::new(false)), channels: HashMap::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CabLink> {
        self.link.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The channels of one frame: speed is the velocity magnitude, attitude from the
    /// quaternion (roll negated), and the velocity rotated by the orientation matrix rows (taken in x, z, y order)
    /// gives the three "accelerations" (×3.6, ×10 for sway and surge) and the traction-loss channel.
    fn write_channels(frame: &CabFrame, t: &mut Telemetry) {
        clear_listener_channels(t);
        let Quaternion { x, y, z, w } = frame.orientation;
        let [vx, vy, vz] = frame.velocity;
        let rows = [
            [1.0 - 2.0 * z * z - 2.0 * y * y, 2.0 * x * z - 2.0 * y * w, 2.0 * x * y + 2.0 * z * w],
            [2.0 * x * z + 2.0 * y * w, 1.0 - 2.0 * x * x - 2.0 * y * y, 2.0 * z * y - 2.0 * x * w],
            [2.0 * x * y - 2.0 * z * w, 2.0 * z * y + 2.0 * x * w, 1.0 - 2.0 * x * x - 2.0 * z * z],
        ];
        let body = |row: [f64; 3]| row[0] * vx + row[1] * vz + row[2] * vy;
        t.speed = (vx * vx + vy * vy + vz * vz).sqrt();
        t.pitch = frame.orientation.pitch_deg();
        t.roll = -frame.orientation.roll_deg();
        t.yaw = frame.orientation.yaw_deg();
        t.lateral_acceleration = -body(rows[0]) * 3.6 * 10.0;
        t.vertical_acceleration = -body(rows[1]) * 3.6;
        t.longitudinal_acceleration = -body(rows[2]) * 3.6 * 10.0;
        t.lateral_velocity = body(rows[0]);
    }
}

impl Default for DynamicCab {
    fn default() -> Self {
        Self::new()
    }
}

impl Source for DynamicCab {
    fn id(&self) -> &'static str {
        "ccd"
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        Some(self.channels.get(name).map_or(LegacyValue::Int(0), |&value| LegacyValue::Float(value)))
    }

    /// The liveness test: the server's running flag.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.lock().running)
    }

    /// Receives the server's latest frame and writes its channels. Before the first frame
    /// the receive step fails (there is no frame yet), which ends the dispatcher's pass.
    fn poll(&mut self, t: &mut Telemetry) -> Result<bool, SourceError> {
        let Some(frame) = self.lock().frame else {
            return Err(DispatcherEnds::at_receive_data(
                "ccd receiveData: AttributeError: 'NoneType' object has no attribute 'car_linear_vel_x'",
            )
            .into());
        };
        Self::write_channels(&frame, t);
        for name in [
            "speed",
            "pitch",
            "roll",
            "yaw",
            "lateral_acceleration",
            "vertical_acceleration",
            "longitudinal_acceleration",
            "lateral_velocity",
        ] {
            self.channels.insert(name, t.get(name).expect("published listener channel"));
        }
        for (name, value) in ["linear_vel_x", "linear_vel_y", "linear_vel_z"].into_iter().zip(frame.velocity) {
            self.channels.insert(name, value);
        }
        Ok(true)
    }
}

impl FlightSource for DynamicCab {
    fn angular_rates(&self) -> AngularRates {
        AngularRates::default()
    }
}

impl Drop for DynamicCab {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// A relay connection: scripted frames in, frames out.
    struct Duplex {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
        fail_writes: bool,
    }

    impl Duplex {
        fn new(packets: &[Vec<u8>]) -> Self {
            let mut input = Vec::new();
            for packet in packets {
                pipe_relay::write_chunk(&mut input, packet).unwrap();
            }
            Duplex { input: io::Cursor::new(input), output: Vec::new(), fail_writes: false }
        }

        fn replies(&self) -> Vec<Vec<u8>> {
            let mut cursor = io::Cursor::new(self.output.clone());
            std::iter::from_fn(|| pipe_relay::read_chunk(&mut cursor).unwrap()).collect()
        }
    }

    impl Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_writes {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn header(kind: i8) -> Vec<u8> {
        let mut packet = 3i16.to_le_bytes().to_vec();
        packet.push(kind as u8);
        packet
    }

    /// A frame packet: the 3-byte header, then one double and 24 floats.
    fn frame_packet(orientation: [f32; 4], velocity: [f32; 3]) -> Vec<u8> {
        let mut packet = header(2);
        packet.extend_from_slice(&1.5f64.to_le_bytes());
        let mut floats = [0f32; 24];
        floats[6..10].copy_from_slice(&orientation);
        floats[10..13].copy_from_slice(&velocity);
        for f in floats {
            packet.extend_from_slice(&f.to_le_bytes());
        }
        packet
    }

    /// Handshake reply, per-frame replies, frame stored, disconnect clears running.
    #[test]
    fn handshake_then_frames() {
        let link = Mutex::new(CabLink::default());
        let mut pipe = Duplex::new(&[header(1), frame_packet([0.0, 0.0, 0.0, 1.0], [3.0, 0.0, 4.0]), header(2)]);
        crate::printed::take();
        assert_eq!(serve_client(&mut pipe, &link), SessionEnd::Disconnected);
        // The 3-byte packet is a frame with too short a body; then the read fails.
        assert_eq!(crate::printed::take(), ["frame error unpack requires a buffer of 104 bytes 0", "error reading 1"]);
        assert_eq!(pipe.replies(), vec![HANDSHAKE_REPLY.to_vec(), FRAME_REPLY.to_vec(), FRAME_REPLY.to_vec()]);
        let state = link.lock().unwrap();
        assert!(!state.running);
        assert_eq!(state.frame.unwrap().velocity, [3.0, 0.0, 4.0]);
    }

    /// A frame before any handshake is answered, then the session ends.
    #[test]
    fn frame_without_handshake_ends_session() {
        let link = Mutex::new(CabLink::default());
        let mut pipe = Duplex::new(&[frame_packet([0.0; 4], [1.0, 1.0, 1.0]), header(1)]);
        assert_eq!(serve_client(&mut pipe, &link), SessionEnd::Disconnected);
        assert_eq!(pipe.replies(), vec![FRAME_REPLY.to_vec()]);
        assert!(link.lock().unwrap().frame.is_some());
    }

    /// A failed reply write clears running; a 2-byte packet stops the server thread.
    #[test]
    fn write_failure_and_short_packet() {
        let link = Mutex::new(CabLink::default());
        let mut pipe = Duplex::new(&[header(1)]);
        pipe.fail_writes = true;
        crate::printed::take();
        assert_eq!(serve_client(&mut pipe, &link), SessionEnd::Disconnected);
        assert_eq!(crate::printed::take(), ["error writing 1"]);
        assert!(!link.lock().unwrap().running);
        let mut short = Duplex::new(&[header(1), vec![3, 0]]);
        assert_eq!(serve_client(&mut short, &link), SessionEnd::ServerStopped);
        assert!(link.lock().unwrap().running);
    }

    /// A session over the bridge's TCP relay (Wine 10): after the pipe-name frame, the handshake and
    /// a frame are answered one-for-one on the same connection, and the session ends when the bridge closes it.
    #[test]
    fn session_over_the_tcp_relay() {
        let relay = pipe_relay::TcpRelay::start("127.0.0.1:0".parse().unwrap()).unwrap();
        let name = format!("OpenRigCueTestCabSession{}", std::process::id());
        let runtime = tempfile::tempdir().unwrap();
        let listener = pipe_relay::RelayListener::bind(runtime.path(), &name);
        let mut bridge = std::net::TcpStream::connect(relay.address()).unwrap();
        bridge.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        pipe_relay::write_chunk(&mut bridge, name.as_bytes()).unwrap();
        pipe_relay::write_chunk(&mut bridge, &header(1)).unwrap();
        pipe_relay::write_chunk(&mut bridge, &frame_packet([0.0, 0.0, 0.0, 1.0], [1.0, 2.0, 2.0])).unwrap();
        bridge.shutdown(std::net::Shutdown::Write).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            if let Some(stream) = listener.accept().unwrap() {
                break stream;
            }
            assert!(std::time::Instant::now() < deadline, "connection routed to the pipe");
            thread::sleep(Duration::from_millis(5));
        };
        let link = Mutex::new(CabLink::default());
        assert_eq!(serve_client(&mut stream, &link), SessionEnd::Disconnected);
        assert_eq!(pipe_relay::read_chunk(&mut bridge).unwrap(), Some(HANDSHAKE_REPLY.to_vec()));
        assert_eq!(pipe_relay::read_chunk(&mut bridge).unwrap(), Some(FRAME_REPLY.to_vec()));
        assert_eq!(link.lock().unwrap().frame.unwrap().velocity, [1.0, 2.0, 2.0]);
        relay.stop();
    }

    /// The pipe server starts with the source: the TCP route exists once `DynamicCab`
    /// is constructed, so a bridge connecting right away (as the service starts its relay right after the
    /// listeners) is served instead of closed as unrouted.
    #[test]
    fn connection_right_after_construction_is_routed() {
        let relay = pipe_relay::TcpRelay::start("127.0.0.1:0".parse().unwrap()).unwrap();
        let name = format!("OpenRigCueTestCabStartup{}", std::process::id());
        let runtime = tempfile::tempdir().unwrap();
        let cab = DynamicCab::serving(runtime.path(), &name);
        assert!(pipe_relay::relay_socket_path_in(runtime.path(), &name).exists(), "Unix socket bound too");
        assert!(pipe_relay::is_routed(&name));
        let mut bridge = std::net::TcpStream::connect(relay.address()).unwrap();
        bridge.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        pipe_relay::write_chunk(&mut bridge, name.as_bytes()).unwrap();
        pipe_relay::write_chunk(&mut bridge, &header(1)).unwrap();
        assert_eq!(pipe_relay::read_chunk(&mut bridge).unwrap(), Some(HANDSHAKE_REPLY.to_vec()));
        drop(bridge);
        drop(cab);
        relay.stop();
    }

    /// Channels: speed magnitude, attitude and the rotated velocities for a level frame.
    #[test]
    fn receive_data_channels() {
        let mut cab = DynamicCab::detached();
        let mut t = Telemetry::off();
        let before_first_frame = cab.poll(&mut t).unwrap_err();
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&before_first_frame));
        assert_eq!(t, Telemetry::off());
        cab.lock().running = true;
        cab.lock().frame = CabFrame::parse(&frame_packet([0.0, 0.0, 0.0, 1.0], [3.0, 4.0, 12.0]));
        assert!(cab.is_running().unwrap());
        assert!(cab.poll(&mut t).unwrap());
        assert_eq!(t.speed, 13.0);
        assert_eq!((t.pitch, t.yaw), (0.0, 0.0));
        // Identity rows: lateral uses vx, vertical vz, longitudinal vy.
        assert_eq!(t.lateral_velocity, 3.0);
        assert_eq!(t.lateral_acceleration, -3.0 * 3.6 * 10.0);
        assert_eq!(t.vertical_acceleration, -12.0 * 3.6);
        assert_eq!(t.longitudinal_acceleration, -4.0 * 3.6 * 10.0);
        assert_eq!(t.rpm, 0.0);
    }

    #[test]
    fn native_dictionary_changes_only_when_receive_publishes_a_frame() {
        use LegacyValue::{Float, Int};
        let mut cab = DynamicCab::detached();
        assert_eq!(cab.native_channel("speed"), Some(Int(0)));
        assert_eq!(cab.native_channel("pitch"), Some(Int(0)));
        cab.lock().frame = CabFrame::parse(&frame_packet([0.0, 0.0, 0.0, 1.0], [3.0, 4.0, 12.0]));
        assert_eq!(cab.native_channel("speed"), Some(Int(0)), "receiver state is not the listener dictionary");
        assert!(cab.poll(&mut Telemetry::off()).unwrap());
        for (name, expected) in [
            ("speed", Float(13.0)),
            ("pitch", Float(0.0)),
            ("linear_vel_x", Float(3.0)),
            ("linear_vel_y", Float(4.0)),
            ("linear_vel_z", Float(12.0)),
            ("lateral_velocity", Float(3.0)),
            ("rpm", Int(0)),
            ("unknown", Int(0)),
        ] {
            assert_eq!(cab.native_channel(name), Some(expected), "{name}");
        }
        cab.lock().frame = CabFrame::parse(&frame_packet([0.0; 4], [0.0; 3]));
        assert_eq!(cab.native_channel("speed"), Some(Float(13.0)));
        cab.poll(&mut Telemetry::off()).unwrap();
        assert_eq!(cab.native_channel("speed"), Some(Float(0.0)));
    }

    /// The rotation rows for a quarter turn about the vertical axis (x, y, z, w) = (0, s, 0, s).
    #[test]
    fn rotated_frame_rows() {
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let frame = CabFrame::parse(&frame_packet([0.0, s, 0.0, s], [1.0, 0.0, 0.0])).unwrap();
        let mut t = Telemetry::off();
        DynamicCab::write_channels(&frame, &mut t);
        let (x, y, z, w) = (0.0, f64::from(s), 0.0, f64::from(s));
        let row0 = [1.0 - 2.0 * z * z - 2.0 * y * y, 2.0 * x * z - 2.0 * y * w, 2.0 * x * y + 2.0 * z * w];
        assert_eq!(t.lateral_velocity, row0[0] * 1.0 + row0[1] * 0.0 + row0[2] * 0.0);
    }
}

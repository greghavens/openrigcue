//! Project Motor Racing (listener `pmracing`): multicast UDP telemetry read by a background receive loop.
//!
//! The receive loop runs in a background thread. While the game is the running
//! game it binds a multicast socket, decodes every datagram into shared state (the connected flag, the player's
//! vehicle id and name, the player's latest vehicle telemetry), and the liveness test and the receive step only
//! report the connected flag. The loop starts when the source is built, and it reads the
//! process-wide `game_running` ([`Parameters`]) whenever it comes round.

mod packets;

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use openrigcue_config::import::value::{Dict, LegacyValue};
use openrigcue_model::Telemetry;

use self::packets::{
    check_race_info, parse_race_state, parse_vehicle_telemetry, Malformed, PacketKind, VehicleTelemetry,
};
use super::forward::{Forwarder, PortForwarding};
use super::parameters::Parameters;
use super::support::channels::zero_listener_channels;
use super::support::net::{Datagrams, Endpoint, Network, SystemNetwork};
use super::support::pymath::{parse_int, round_to, trunc};
use super::support::quaternion;
use crate::{Source, SourceError};

/// The default multicast port.
pub const DEFAULT_PORT: u16 = 7576;
/// The multicast group.
const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 150);
/// Receive buffer size.
const RECEIVE_SIZE: usize = 4096;
/// Receive timeout of the multicast socket.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
/// Pause at the end of every pass of the outer loop.
const RETRY_PAUSE: Duration = Duration::from_secs(2);
/// The `game_running` values the loop serves.
const GAME_IDS: &[&str] = &["pmracing"];
/// The text whose characters the port lookup tries as keys.
const PORT_KEY_CHARACTERS: &str = "pmracing";

/// Configuration the listener reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PmRacingSettings {
    /// Canonical `NETWORK_PORT` values, including present invalid entries.
    pub network_ports: Dict,
}

/// Reads NETWORK_PORT at the background worker's socket-creation boundary, including
/// retries after the main telemetry dispatcher has stopped.
pub type NetworkPortsReader = Arc<dyn Fn() -> Dict + Send + Sync>;

/// The port lookup.
///
/// It looks up the single
/// characters `p`, `m`, `r`, `a`, `c`, `i`, `n`, `g` in `NETWORK_PORT` and never the key `pmracing`. The
/// first character present decides: its value as an integer, or the default port when it is not one. With
/// none present the result is the default port.
fn resolve_port(network_ports: &Dict) -> i64 {
    PORT_KEY_CHARACTERS
        .chars()
        .find_map(|c| network_ports.get(c.encode_utf8(&mut [0; 4]) as &str))
        .and_then(|value| match value {
            LegacyValue::Str(text) => port_integer_text(text),
            LegacyValue::Bytes(bytes) if bytes.is_ascii() => port_integer_text(std::str::from_utf8(bytes).ok()?),
            other => other.py_int(),
        })
        .unwrap_or(i64::from(DEFAULT_PORT))
}

fn port_integer_text(text: &str) -> Option<i64> {
    let normalized = openrigcue_model::python_numeric::python_int_text(text)?;
    // A valid int outside i64 still reaches bind's range failure; it must not turn
    // into the default port as an invalid integer would.
    Some(parse_int(&normalized).unwrap_or_else(|| if normalized.starts_with('-') { i64::MIN } else { i64::MAX }))
}

/// Creates the multicast socket: `SO_REUSEADDR`, bind every interface at the port, join `224.0.0.150` on
/// `INADDR_ANY`, 1 s timeout. A port outside 0–65535 makes the bind fail.
fn multicast_endpoint(port: i64) -> Result<Endpoint, Fault> {
    let port = u16::try_from(port).map_err(|_| Fault::PortOutOfRange(port))?;
    Ok(Endpoint {
        bind: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
        timeout: Some(RECEIVE_TIMEOUT),
        broadcast: false,
        reuse_address: true,
        multicast_group: Some(MULTICAST_GROUP),
    })
}

/// What ends a pass of the receive loop with the printed `pmracing_main_loop error:` message.
#[derive(Debug, thiserror::Error)]
enum Fault {
    #[error("port {0} out of range")]
    PortOutOfRange(i64),
    #[error("socket: {0}")]
    Socket(#[from] io::Error),
    #[error("packet: {0}")]
    Packet(#[from] Malformed),
}

impl Fault {
    /// The printed text of the failure: an operating-system
    /// error, or the bind range check. A malformed packet has its own description.
    fn python_text(&self) -> String {
        match self {
            Fault::PortOutOfRange(_) => "bind(): port must be 0-65535.".into(),
            Fault::Socket(error) => crate::printed::os_error(error),
            Fault::Packet(_) => self.to_string(),
        }
    }
}

/// The state the receive loop writes and channel reads use.
#[derive(Debug, Default)]
struct Feed {
    connected: bool,
    player_id: Option<i32>,
    player_name: Option<String>,
    /// The player's latest telemetry.
    telemetry: Option<Arc<VehicleTelemetry>>,
}

/// Whether the loop should end.
#[derive(Debug, Default)]
struct Control {
    shut_down: bool,
}

/// State shared by the source and its receive thread.
#[derive(Debug)]
struct Link {
    feed: Mutex<Feed>,
    control: Mutex<Control>,
    wake: Condvar,
    /// The UDP forwarder.
    forwarder: Forwarder,
    /// The process-wide parameters (`game_running`).
    parameters: Arc<Parameters>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Link {
    fn new(parameters: Arc<Parameters>, port_forwarding: Arc<PortForwarding>) -> Self {
        let forwarder = Forwarder::with(parameters.clone(), port_forwarding);
        Link { feed: Mutex::default(), control: Mutex::default(), wake: Condvar::new(), forwarder, parameters }
    }

    fn feed(&self) -> MutexGuard<'_, Feed> {
        lock(&self.feed)
    }

    fn set_connected(&self, connected: bool) {
        self.feed().connected = connected;
    }

    /// Whether the running game is one this loop serves, read at call time.
    fn is_active(&self) -> bool {
        !self.is_shut_down() && self.parameters.game_running_is_one_of(GAME_IDS)
    }

    fn is_shut_down(&self) -> bool {
        lock(&self.control).shut_down
    }

    fn shut_down(&self) {
        lock(&self.control).shut_down = true;
        self.wake.notify_all();
    }

    /// A pause of `duration`, cut short only by [`Link::shut_down`].
    fn pause(&self, duration: Duration) {
        let control = lock(&self.control);
        drop(self.wake.wait_timeout_while(control, duration, |c| !c.shut_down).unwrap_or_else(PoisonError::into_inner));
    }
}

/// The receive loop.
struct ReceiveLoop {
    net: Box<dyn Network>,
    network_ports: NetworkPortsReader,
    link: Arc<Link>,
}

impl ReceiveLoop {
    /// The outer loop: serve while the game runs, log and drop the
    /// connection on any failure, then sleep 2 s, until the source is dropped.
    fn run(mut self) {
        while !self.link.is_shut_down() {
            if self.link.is_active() {
                if let Err(fault) = self.serve() {
                    // Prints "pmracing_main_loop error:" and the failure text.
                    crate::printed::line(format!("pmracing_main_loop error: {}", fault.python_text()));
                    self.link.set_connected(false);
                }
            }
            self.link.pause(RETRY_PAUSE);
        }
    }

    /// One pass of the outer loop while the game runs: open the socket, reset the session state, handle
    /// datagrams until the game stops or a receive fails, then close and report "not connected".
    fn serve(&mut self) -> Result<(), Fault> {
        let endpoint = multicast_endpoint(resolve_port(&(self.network_ports)()))?;
        let mut socket = self.net.bind(&endpoint)?;
        {
            // The connected flag, the player's id and name, the player's telemetry and
            // the last race info are reset in one atomic step,
            // so no getter sees a part of the reset.
            let mut feed = self.link.feed();
            feed.connected = false;
            feed.player_id = None;
            feed.telemetry = None;
            feed.player_name = None;
        }
        while self.link.is_active() {
            if !self.receive(socket.as_mut())? {
                break;
            }
        }
        drop(socket);
        self.link.set_connected(false);
        Ok(())
    }

    /// One receive: a timeout reports "not connected" and keeps the socket; any other receive error
    /// reports "not connected" and ends the pass (`false`).
    fn receive(&mut self, socket: &mut dyn Datagrams) -> Result<bool, Fault> {
        match socket.receive(RECEIVE_SIZE) {
            Ok((data, _)) => {
                self.handle(&data)?;
                Ok(true)
            }
            Err(e) => {
                self.link.set_connected(false);
                Ok(matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
            }
        }
    }

    /// Handles one datagram: ignore empty datagrams, forward, mark
    /// connected, then dispatch on the packet type (unknown types are skipped). Race states name the
    /// player's car; telemetry packets are decoded only once the player is known and kept only for it.
    fn handle(&mut self, data: &[u8]) -> Result<(), Malformed> {
        let Some(&type_byte) = data.first() else { return Ok(()) };
        self.link.forwarder.forward(data);
        self.link.set_connected(true);
        match PacketKind::from_byte(type_byte) {
            Some(PacketKind::RaceInfo) => check_race_info(data)?,
            Some(PacketKind::ParticipantRaceState) => {
                let state = parse_race_state(data)?;
                if state.is_player {
                    publish_player(&self.link, state);
                }
            }
            Some(PacketKind::ParticipantVehicleTelemetry) => {
                if self.link.feed().player_id.is_some() {
                    let telemetry = parse_vehicle_telemetry(data)?;
                    let player_id = self.link.feed().player_id;
                    if Some(telemetry.vehicle_id) == player_id {
                        self.link.feed().telemetry = Some(Arc::new(telemetry));
                    }
                }
            }
            Some(PacketKind::SessionStopped) | None => {}
        }
        Ok(())
    }
}

/// Stores the player's vehicle id and name,
/// published together.
fn publish_player(link: &Link, state: packets::RaceState) {
    let mut feed = link.feed();
    feed.player_id = Some(state.vehicle_id);
    feed.player_name = Some(state.vehicle_name);
}

/// Numeric compatibility projection. Live typed reads below retain the rpm's integer text,
/// integer defaults and per-call access to the receive loop's latest state.
fn write_channels(feed: &Feed, t: &mut Telemetry) {
    zero_listener_channels(t);
    let Some(car) = feed.telemetry.as_ref().filter(|_| feed.connected) else { return };
    let int_or_zero = |x: f64| trunc(x).unwrap_or(0.0);
    let round6 = |x: f64| round_to(x, 6);
    t.rpm = int_or_zero(car.engine_rpm);
    // The telemetry has no maximum engine rpm, so max_rpm stays 0.
    t.gear = f64::from(car.gear);
    t.speed = int_or_zero(car.overall_speed);
    if let Some(name) = feed.player_name.as_ref().filter(|name| !name.is_empty()) {
        t.vehicle.clone_from(name);
    }
    let wheel = |i: usize| car.wheels.get(i);
    let slip = |i: usize| wheel(i).map_or(0.0, |w| w.slip_ratio.abs());
    let strain = |i: usize| wheel(i).map_or(0.0, |w| w.spring_strain);
    (t.wheel_slip_lf, t.wheel_slip_rf, t.wheel_slip_lr, t.wheel_slip_rr) = (slip(0), slip(1), slip(2), slip(3));
    (t.suspension_travel_lf, t.suspension_travel_rf, t.suspension_travel_lr, t.suspension_travel_rr) =
        (strain(0), strain(1), strain(2), strain(3));
    let acceleration = car.acceleration_local;
    t.lateral_acceleration = round6(acceleration.z);
    t.vertical_acceleration = round6(acceleration.y);
    t.longitudinal_acceleration = round6(acceleration.x);
    let [qx, qy, qz, qw] = car.quaternion;
    t.yaw = quaternion::yaw(qx, qy, qz, qw);
    t.roll = -quaternion::roll(qx, qy, qz, qw);
    t.pitch = quaternion::pitch(qx, qy, qz, qw);
    let velocity = car.velocity_local;
    t.lateral_velocity = round6(velocity.z);
    t.longitudinal_velocity = round6(velocity.x);
    t.vertical_velocity = round6(velocity.y);
}

/// A channel read: each channel is evaluated only when requested, after the common connected/player-telemetry
/// gate. Packet fields are ordinary f32/i32 values.
///
/// The read is atomic with respect to the receive loop, which cannot run
/// in the middle of it: one view of the shared state serves the whole call. The yaw channel
/// therefore cannot fail (the gated player always has
/// its four-number quaternion).
fn native_read(feed: &Feed, name: &str) -> LegacyValue {
    match feed.telemetry.as_deref().filter(|_| feed.connected) {
        Some(car) => read_fields(car, name, feed.player_name.as_deref()),
        None => LegacyValue::Int(0),
    }
}

fn read_fields(car: &VehicleTelemetry, name: &str, player_name: Option<&str>) -> LegacyValue {
    if name == "car_name" {
        return match player_name {
            Some(name) if !name.is_empty() => LegacyValue::Str(name.to_owned()),
            _ => LegacyValue::Int(0),
        };
    }
    let wheel = match name {
        "wheel_slip_lf" | "suspension_travel_lf" => Some(0),
        "wheel_slip_rf" | "suspension_travel_rf" => Some(1),
        "wheel_slip_lr" | "suspension_travel_lr" => Some(2),
        "wheel_slip_rr" | "suspension_travel_rr" => Some(3),
        _ => None,
    };
    if let Some(index) = wheel {
        return car.wheels.get(index).map_or(LegacyValue::Int(0), |wheel| {
            LegacyValue::Float(if name.starts_with("wheel_slip_") {
                wheel.slip_ratio.abs()
            } else {
                wheel.spring_strain
            })
        });
    }
    let rounded = |value| LegacyValue::Float(round_to(value, 6));
    let [x, y, z, w] = car.quaternion;
    match name {
        "rpm" => trunc(car.engine_rpm).map_or(LegacyValue::Int(0), |value| {
            // The integer value of -0.0 is 0; finite packet f32 magnitudes can exceed i64.
            LegacyValue::Str(if value == 0.0 { "0".into() } else { format!("{value:.0}") })
        }),
        // The telemetry has no maximum engine rpm; max_rpm answers 0.
        "max_rpm" => LegacyValue::Int(0),
        "gear" => LegacyValue::Str(car.gear.to_string()),
        "speed" => trunc(car.overall_speed).map_or(LegacyValue::Int(0), |value| {
            if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value) {
                LegacyValue::Int(value as i64)
            } else {
                // LegacyValue's integers are bounded. Keep the existing finite numeric
                // compatibility magnitude rather than inventing a failed int conversion.
                LegacyValue::Float(value)
            }
        }),
        "lateral_acceleration" => rounded(car.acceleration_local.z),
        "vertical_acceleration" => rounded(car.acceleration_local.y),
        "longitudinal_acceleration" => rounded(car.acceleration_local.x),
        "yaw" => LegacyValue::Float(quaternion::yaw(x, y, z, w)),
        "roll" => LegacyValue::Float(-quaternion::roll(x, y, z, w)),
        "pitch" => LegacyValue::Float(quaternion::pitch(x, y, z, w)),
        "lateral_velocity" => rounded(car.velocity_local.z),
        "longitudinal_velocity" => rounded(car.velocity_local.x),
        "vertical_velocity" => rounded(car.velocity_local.y),
        _ => LegacyValue::Int(0),
    }
}

/// The Project Motor Racing listener.
pub struct PmRacingSource {
    link: Arc<Link>,
}

impl PmRacingSource {
    /// Starts the receive loop.
    pub fn new(settings: PmRacingSettings) -> Self {
        Self::with_network(settings, Box::new(SystemNetwork))
    }

    /// Production configuration is shared with the UI. Read it on each new socket,
    /// without depending on a dispatcher prepare or liveness call.
    pub fn with_live_network_ports(reader: NetworkPortsReader) -> Self {
        Self::with_reader_parts(reader, Box::new(SystemNetwork), Parameters::for_listener())
    }

    fn with_network(settings: PmRacingSettings, net: Box<dyn Network>) -> Self {
        Self::with_parts(settings, net, Parameters::for_listener())
    }

    fn with_parts(settings: PmRacingSettings, net: Box<dyn Network>, parameters: Arc<Parameters>) -> Self {
        Self::with_reader_parts(Arc::new(move || settings.network_ports.clone()), net, parameters)
    }

    fn with_reader_parts(
        network_ports: NetworkPortsReader,
        net: Box<dyn Network>,
        parameters: Arc<Parameters>,
    ) -> Self {
        let link = Arc::new(Link::new(parameters, PortForwarding::for_listener()));
        let receive_loop = ReceiveLoop { net, network_ports, link: Arc::clone(&link) };
        if let Err(error) = thread::Builder::new().name("pmracing_main_loop".into()).spawn(move || receive_loop.run()) {
            tracing::warn!("pmracing: cannot start the receive loop: {error}");
        }
        PmRacingSource { link }
    }
}

impl Source for PmRacingSource {
    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        Some(native_read(&self.link.feed(), name))
    }

    fn id(&self) -> &'static str {
        "pmracing"
    }

    /// The liveness test: the connected flag.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        Ok(self.link.feed().connected)
    }

    /// The receive step answers the connected flag without evaluating any getters.
    fn receive_native(&mut self, _: &mut Telemetry) -> Result<bool, SourceError> {
        Ok(self.link.feed().connected)
    }

    /// Numeric compatibility API for callers that explicitly request a projected frame.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let feed = self.link.feed();
        write_channels(&feed, telemetry);
        Ok(feed.connected)
    }
}

impl Drop for PmRacingSource {
    fn drop(&mut self) {
        self.link.shut_down();
    }
}

#[cfg(test)]
mod tests {
    use super::packets::build::*;
    use super::packets::{Vec3, Wheel};
    use super::*;
    use crate::udp_racing::support::net::fake::{FakeNetwork, ForwardSink};
    use std::time::Instant;

    #[test]
    fn retry_reads_current_ports_only_when_creating_the_next_socket() {
        let net = FakeNetwork::default();
        let ports = Arc::new(Mutex::new(settings(&[("p", "9001")]).network_ports));
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader: NetworkPortsReader = {
            let ports = ports.clone();
            let reads = reads.clone();
            Arc::new(move || {
                reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                lock(&ports).clone()
            })
        };
        let parameters = Arc::new(Parameters::default());
        parameters.set_game_running("pmracing");
        let link = Arc::new(Link::new(parameters, Arc::default()));
        let mut worker = ReceiveLoop { net: Box::new(net.clone()), network_ports: reader, link };
        for (value, expected) in [
            (LegacyValue::from("9001"), Some(9001)),
            (LegacyValue::from("9002"), Some(9002)),
            (LegacyValue::from("invalid"), Some(DEFAULT_PORT)),
            (LegacyValue::from("999999999999999999999999"), None),
            (LegacyValue::Bytes(b"9003".to_vec()), Some(9003)),
        ] {
            lock(&ports).insert("p", value);
            if let Some(port) = expected {
                net.push_failure(port);
                worker.serve().unwrap();
                assert_eq!(net.bound().last().unwrap().bind.port(), port);
            } else {
                let bound = net.bound().len();
                assert!(matches!(worker.serve(), Err(Fault::PortOutOfRange(_))));
                assert_eq!(net.bound().len(), bound, "valid wide int fails before binding a default socket");
            }
            assert!(!worker.link.feed().connected);
        }
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[test]
    fn native_port_values_keep_the_first_present_key_and_python_conversion() {
        let mut ports = Dict::new();
        ports.insert("m", LegacyValue::Int(9002));
        for (value, expected) in [
            (LegacyValue::Float(9001.9), 9001),
            (LegacyValue::Bool(true), 1),
            (LegacyValue::Int(-1), -1),
            (LegacyValue::None, i64::from(DEFAULT_PORT)),
            (LegacyValue::List(vec![LegacyValue::Int(9000)]), i64::from(DEFAULT_PORT)),
            (LegacyValue::Str("invalid".into()), i64::from(DEFAULT_PORT)),
        ] {
            ports.insert("p", value);
            assert_eq!(resolve_port(&ports), expected);
        }
        ports.insert("p", LegacyValue::Int(-1));
        assert!(matches!(multicast_endpoint(resolve_port(&ports)), Err(Fault::PortOutOfRange(-1))));
        ports.insert("p", LegacyValue::Str(format!("{}7577", "0".repeat(4296))));
        assert_eq!(resolve_port(&ports), 7577);
        ports.insert("p", LegacyValue::Str(format!("{}7577", "0".repeat(4297))));
        assert_eq!(resolve_port(&ports), i64::from(DEFAULT_PORT));
    }

    fn settings(ports: &[(&str, &str)]) -> PmRacingSettings {
        PmRacingSettings {
            network_ports: ports
                .iter()
                .map(|(k, v)| {
                    (openrigcue_config::import::value::Key::Str(k.to_string()), LegacyValue::Str(v.to_string()))
                })
                .collect(),
        }
    }

    /// A receive loop while `game_running == 'pmracing'`, whose datagrams the forwarder sends to `sink` while
    /// `listener_running` is `listener`.
    fn receive_loop_with(net: &FakeNetwork, listener: &str, sink: &ForwardSink) -> ReceiveLoop {
        let parameters = Arc::new(Parameters::default());
        parameters.set_game_running("pmracing");
        let port_forwarding = Arc::new(PortForwarding::default());
        port_forwarding.set_entries("pmracing", vec![sink.target()]);
        parameters.set_listener_running(listener);
        let link = Arc::new(Link::new(parameters, port_forwarding));
        ReceiveLoop { net: Box::new(net.clone()), network_ports: Arc::new(Dict::new), link }
    }

    fn receive_loop(net: &FakeNetwork) -> ReceiveLoop {
        receive_loop_with(net, "none", &ForwardSink::new())
    }

    fn race_state(vehicle_id: i32, is_player: u8, name: &[u8]) -> Vec<u8> {
        let d = Datagram::new(1).i32(vehicle_id).u8(is_player).pascal(name).pascal(b"").pascal(b"").pascal(b"");
        d.i32(1).i32(1).floats(&[0.0; 3]).i32(0).u8(0).u8(0).u8(0).u8(0).u8(0).u32(0).0
    }

    fn vehicle_telemetry(vehicle_id: i32, engine_rpm: f32) -> Vec<u8> {
        let mut d = Datagram::new(2).i32(vehicle_id).u8(4);
        for i in 0..4u8 {
            d = wheel(d, -0.25 * f32::from(i + 1), 0.5 * f32::from(i));
        }
        let b = Body {
            quaternion: [0.1, 0.2, 0.3, 0.9],
            velocity_local: [1.25, -2.5, 3.0000005],
            acceleration_local: [0.1, 0.2, -0.3],
            overall_speed: 33.9,
            engine_rpm,
        };
        controls(body(d, &b).u8(0).u8(0).f32(0.5), 3).0
    }

    #[test]
    fn port_lookup_uses_single_characters() {
        // The port lookup tries the characters of 'pmracing'; the key 'pmracing' itself is never read.
        assert_eq!(resolve_port(&settings(&[]).network_ports), 7576);
        assert_eq!(resolve_port(&settings(&[("pmracing", "9000")]).network_ports), 7576);
        assert_eq!(resolve_port(&settings(&[("m", "9001"), ("g", "9002")]).network_ports), 9001);
        assert_eq!(resolve_port(&settings(&[("p", "9003"), ("m", "9001")]).network_ports), 9003);
        // A value that is not an integer gives the default port without trying later characters.
        assert_eq!(resolve_port(&settings(&[("p", "x"), ("m", "9001")]).network_ports), 7576);
    }

    #[test]
    fn multicast_socket_options() {
        // The multicast socket: SO_REUSEADDR, bind every interface, IP_ADD_MEMBERSHIP 224.0.0.150, 1 s timeout.
        let e = multicast_endpoint(7576).unwrap();
        assert_eq!(e.bind, "0.0.0.0:7576".parse().unwrap());
        assert_eq!(e.multicast_group, Some(Ipv4Addr::new(224, 0, 0, 150)));
        assert!(e.reuse_address && !e.broadcast);
        assert_eq!(e.timeout, Some(Duration::from_secs(1)));
        assert!(matches!(multicast_endpoint(70_000), Err(Fault::PortOutOfRange(70_000))));
        assert!(matches!(multicast_endpoint(-1), Err(Fault::PortOutOfRange(-1))));
    }

    #[test]
    fn player_telemetry_is_kept_for_the_player_car_only() {
        // Receive loop: telemetry before any player race state is ignored; then only telemetry of the
        // player's vehicle id replaces the player's telemetry. A failed receive closes the socket.
        let net = FakeNetwork::default();
        net.push(DEFAULT_PORT, &vehicle_telemetry(5, 1000.0));
        net.push(DEFAULT_PORT, &race_state(4, 0, b"Other"));
        net.push(DEFAULT_PORT, &race_state(5, 1, b"Ginetta"));
        net.push(DEFAULT_PORT, &vehicle_telemetry(4, 2000.0));
        net.push(DEFAULT_PORT, &vehicle_telemetry(5, 3000.0));
        net.push(DEFAULT_PORT, &vehicle_telemetry(6, 4000.0));
        net.push_failure(DEFAULT_PORT);
        let mut rl = receive_loop(&net);
        rl.serve().unwrap();
        let feed = rl.link.feed();
        assert_eq!(feed.player_id, Some(5));
        assert_eq!(feed.player_name.as_deref(), Some("Ginetta"));
        assert_eq!(feed.telemetry.as_ref().unwrap().engine_rpm, 3000.0);
        assert!(!feed.connected);
        assert_eq!(net.bound()[0].multicast_group, Some(MULTICAST_GROUP));
    }

    #[test]
    fn a_new_socket_resets_the_session() {
        // Receive loop: after the socket is created, the connected flag, the player's id and name and
        // the player's telemetry are reset.
        let net = FakeNetwork::default();
        let mut rl = receive_loop(&net);
        rl.link.feed().player_id = Some(1);
        rl.link.feed().player_name = Some("old".into());
        net.push_failure(DEFAULT_PORT);
        rl.serve().unwrap();
        assert_eq!(rl.link.feed().player_id, None);
        assert_eq!(rl.link.feed().player_name, None);
    }

    #[test]
    fn datagram_handling_edge_cases() {
        // Receive loop: an empty datagram is skipped before connecting; an unknown packet type is
        // skipped after connecting; a malformed packet ends the pass.
        let net = FakeNetwork::default();
        let mut rl = receive_loop(&net);
        rl.handle(&[]).unwrap();
        assert!(!rl.link.feed().connected);
        rl.handle(&[9, 1, 2]).unwrap();
        assert!(rl.link.feed().connected);
        rl.handle(&[2, 0, 0]).unwrap(); // telemetry is not decoded while the player is unknown
        assert_eq!(rl.handle(&[0, 0, 0]), Err(Malformed::Short));
        assert_eq!(rl.handle(&race_state(1, 1, b"x")[..20]), Err(Malformed::Short));
    }

    #[test]
    fn malformed_packets_end_the_pass_with_an_error() {
        // Receive loop: decode failures end the pass.
        let net = FakeNetwork::default();
        net.push(DEFAULT_PORT, &[0, 1, 0, 5]);
        let mut rl = receive_loop(&net);
        assert!(matches!(rl.serve(), Err(Fault::Packet(Malformed::Short))));
        net.refuse_bind(true);
        assert!(matches!(rl.serve(), Err(Fault::Socket(_))));
    }

    #[test]
    fn timeouts_report_not_connected_and_keep_the_socket() {
        // Receive loop: a timeout clears the connected flag and continues.
        let net = FakeNetwork::default();
        net.push(DEFAULT_PORT, &[3, 0, 0]);
        net.push_timeout(DEFAULT_PORT);
        let mut rl = receive_loop(&net);
        let mut socket = rl.net.bind(&multicast_endpoint(7576).unwrap()).unwrap();
        assert!(rl.receive(socket.as_mut()).unwrap());
        assert!(rl.link.feed().connected);
        assert!(rl.receive(socket.as_mut()).unwrap());
        assert!(!rl.link.feed().connected);
        net.push_failure(DEFAULT_PORT);
        assert!(!rl.receive(socket.as_mut()).unwrap());
    }

    #[test]
    fn forwarding_follows_the_running_listener() {
        // The forwarder forwards only while listener_running is this listener.
        let sink = ForwardSink::new();
        let mut rl = receive_loop_with(&FakeNetwork::default(), "dirt4", &sink);
        rl.handle(&[3, 0, 1]).unwrap();
        rl.link.parameters.set_listener_running("pmracing");
        rl.handle(&[3, 0, 2]).unwrap();
        assert_eq!(sink.received(), vec![vec![3, 0, 2]]);
    }

    fn connected_feed() -> Feed {
        let wheels =
            vec![Wheel { slip_ratio: -0.5, spring_strain: 0.01 }, Wheel { slip_ratio: 0.25, spring_strain: 0.02 }];
        Feed {
            connected: true,
            player_id: Some(5),
            player_name: Some("Ginetta G56".into()),
            telemetry: Some(Arc::new(VehicleTelemetry {
                vehicle_id: 5,
                wheels,
                quaternion: [0.1, 0.2, 0.3, 0.9],
                velocity_local: Vec3 { x: 1.0000004, y: -2.5, z: 3.0000006 },
                acceleration_local: Vec3 { x: 0.1234564, y: 0.2, z: -9.8765435 },
                overall_speed: -33.9,
                engine_rpm: 6999.99,
                gear: -1,
            })),
        }
    }

    fn inactive_source() -> PmRacingSource {
        PmRacingSource { link: Arc::new(Link::new(Arc::default(), Arc::default())) }
    }

    #[test]
    fn native_getters_follow_packet_producers_and_defaults() {
        let source = inactive_source();
        let mut producer = ReceiveLoop {
            net: Box::<FakeNetwork>::default(),
            network_ports: Arc::new(Dict::new),
            link: source.link.clone(),
        };
        for name in ["rpm", "gear", "speed", "car_name", "yaw_rate", "unknown"] {
            assert_eq!(source.native_channel(name), Some(LegacyValue::Int(0)), "idle {name}");
        }
        producer.handle(&[3, 0, 0]).unwrap();
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(0)), "no player telemetry");
        producer.handle(&race_state(5, 1, b"Ginetta\xff")).unwrap();
        producer.handle(&vehicle_telemetry(4, 8000.0)).unwrap();
        assert_eq!(source.native_channel("car_name"), Some(LegacyValue::Int(0)), "wrong vehicle");
        producer.handle(&vehicle_telemetry(5, 6999.99)).unwrap();
        for (name, expected) in [
            ("rpm", LegacyValue::from("6999")),
            ("max_rpm", LegacyValue::Int(0)),
            ("gear", LegacyValue::from("3")),
            ("speed", LegacyValue::Int(33)),
            ("car_name", LegacyValue::from("Ginetta")),
            ("wheel_slip_lf", LegacyValue::Float(0.25)),
            ("wheel_slip_rf", LegacyValue::Float(0.5)),
            ("wheel_slip_lr", LegacyValue::Float(0.75)),
            ("wheel_slip_rr", LegacyValue::Float(1.0)),
            ("suspension_travel_lf", LegacyValue::Float(0.0)),
            ("suspension_travel_rf", LegacyValue::Float(0.5)),
            ("suspension_travel_lr", LegacyValue::Float(1.0)),
            ("suspension_travel_rr", LegacyValue::Float(1.5)),
            ("lateral_acceleration", LegacyValue::Float(-0.3)),
            ("vertical_acceleration", LegacyValue::Float(0.2)),
            ("longitudinal_acceleration", LegacyValue::Float(0.1)),
            ("lateral_velocity", LegacyValue::Float(3.0)),
            ("longitudinal_velocity", LegacyValue::Float(1.25)),
            ("vertical_velocity", LegacyValue::Float(-2.5)),
            ("yaw_rate", LegacyValue::Int(0)),
            ("track_loc", LegacyValue::Int(0)),
            ("unknown", LegacyValue::Int(0)),
        ] {
            assert_eq!(source.native_channel(name), Some(expected), "{name}");
        }
        let [x, y, z, w] = source.link.feed().telemetry.as_ref().unwrap().quaternion;
        for (name, value) in [
            ("yaw", quaternion::yaw(x, y, z, w)),
            ("roll", -quaternion::roll(x, y, z, w)),
            ("pitch", quaternion::pitch(x, y, z, w)),
        ] {
            assert_eq!(source.native_channel(name), Some(LegacyValue::Float(value)), "{name}");
        }
    }

    #[test]
    fn native_getters_retain_live_values_after_receive() {
        use crate::derive::{Listener, SourceListener};
        let mut source = inactive_source();
        *source.link.feed() = connected_feed();
        let mut frame = Telemetry::off();
        frame.rpm = 123.0;
        frame.vehicle = "retained compatibility frame".into();
        assert!(source.receive_native(&mut frame).unwrap());
        assert_eq!(frame.rpm, 123.0, "receiveData only returns connected");
        assert_eq!(frame.vehicle, "retained compatibility frame");
        let link = source.link.clone();
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::from("6999")));
        {
            let mut feed = link.feed();
            Arc::make_mut(feed.telemetry.as_mut().unwrap()).engine_rpm = 4321.9;
            feed.player_name = Some("new car".into());
        }
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::from("4321")));
        assert_eq!(listener.vehicle().as_deref(), Some("new car"));
        link.set_connected(false);
        assert_eq!(listener.native_channel("rpm"), Some(LegacyValue::Int(0)));
        assert_eq!(listener.native_channel("speed"), Some(LegacyValue::Int(0)));
        assert_eq!(listener.vehicle().as_deref(), Some(""));
    }

    /// The gate and the reads of one getter use one view of the shared state: disconnected or without a player
    /// every name answers 0, and the yaw channel never meets a missing player.
    #[test]
    fn a_getter_reads_one_view_of_the_globals() {
        let mut feed = connected_feed();
        crate::printed::take();
        assert_eq!(native_read(&feed, "rpm"), LegacyValue::from("6999"));
        assert_ne!(native_read(&feed, "yaw"), LegacyValue::Int(0));
        feed.connected = false;
        assert_eq!(native_read(&feed, "rpm"), LegacyValue::Int(0));
        feed.connected = true;
        feed.telemetry = None;
        for name in ["rpm", "yaw", "roll", "car_name", "unknown"] {
            assert_eq!(native_read(&feed, name), LegacyValue::Int(0), "{name}");
        }
        assert!(crate::printed::take().is_empty());
    }

    /// The player's vehicle name when it is not empty, else 0.
    #[test]
    fn native_name_is_the_player_name_or_zero() {
        let mut feed = connected_feed();
        feed.player_name = Some("after".into());
        assert_eq!(native_read(&feed, "car_name"), LegacyValue::from("after"));
        for missing in [None, Some(String::new())] {
            feed.player_name = missing;
            assert_eq!(native_read(&feed, "car_name"), LegacyValue::Int(0));
        }
    }

    /// The player's id and name are published together, with
    /// the previous player's telemetry left in place.
    #[test]
    fn player_id_and_name_are_published_together_and_prior_telemetry_survives() {
        let source = inactive_source();
        *source.link.feed() = connected_feed();
        publish_player(
            &source.link,
            packets::RaceState { vehicle_id: 99, is_player: true, vehicle_name: "New car".into() },
        );
        {
            let feed = source.link.feed();
            assert_eq!((feed.player_id, feed.player_name.as_deref()), (Some(99), Some("New car")));
            assert_eq!(feed.telemetry.as_ref().unwrap().vehicle_id, 5);
        }
        assert_eq!(source.native_channel("car_name"), Some(LegacyValue::from("New car")));
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::from("6999")));
    }

    /// Whenever a getter holds the shared state, the player's id and name belong to the same race-state packet.
    #[test]
    fn a_getter_never_sees_a_player_id_without_its_name() {
        let source = inactive_source();
        let link = source.link.clone();
        let writer = std::thread::spawn(move || {
            for round in 0..20_000 {
                let id = round % 5;
                publish_player(
                    &link,
                    packets::RaceState { vehicle_id: id, is_player: true, vehicle_name: id.to_string() },
                );
            }
        });
        while !writer.is_finished() {
            let feed = source.link.feed();
            assert_eq!(feed.player_id.map(|id| id.to_string()), feed.player_name.clone());
        }
        writer.join().unwrap();
    }

    #[test]
    fn native_getters_keep_local_failures_and_number_types() {
        let source = inactive_source();
        *source.link.feed() = connected_feed();
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            {
                let mut feed = source.link.feed();
                let car = Arc::make_mut(feed.telemetry.as_mut().unwrap());
                car.engine_rpm = invalid;
                car.overall_speed = invalid;
                car.wheels.clear();
                feed.player_name = Some(String::new());
            }
            for name in ["rpm", "speed", "car_name", "wheel_slip_lf", "suspension_travel_rr"] {
                assert_eq!(source.native_channel(name), Some(LegacyValue::Int(0)), "{name}");
            }
            assert_eq!(source.native_channel("longitudinal_velocity"), Some(LegacyValue::Float(1.0)));
        }
        for (rpm, expected) in
            [(-0.0, "0"), (-12.9, "-12"), (f64::from(f32::MAX), "340282346638528859811704183484516925440")]
        {
            Arc::make_mut(source.link.feed().telemetry.as_mut().unwrap()).engine_rpm = rpm;
            assert_eq!(source.native_channel("rpm"), Some(LegacyValue::from(expected)));
        }
    }

    #[test]
    fn native_getters_reach_dispatcher_as_native_values() {
        use crate::derive::*;
        struct Games;
        impl GameDirectory for Games {
            fn is_flying(&self, _: &str) -> bool {
                false
            }
            fn intellitune_reference(&self, _: &str) -> Option<IntellituneReference> {
                None
            }
        }
        let source = inactive_source();
        *source.link.feed() = connected_feed();
        let mut dispatcher = Dispatcher::new(vec![Box::new(SourceListener::new(source))]);
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
        let hits = [ScanHit { game_id: "pmracing".into(), listener_id: "pmracing".into() }];
        dispatcher.tick(&TickInput {
            hits: &hits,
            settings: &settings,
            profile: &profile,
            runtime: &RuntimeInputs::default(),
            games: &Games,
        });
        assert!(!dispatcher.has_ended());
        assert_eq!(dispatcher.telemetry().native_value("rpm"), Some(&LegacyValue::Int(6999)));
        assert_eq!(dispatcher.telemetry().native_value("max_rpm"), Some(&LegacyValue::Int(0)));
        assert_eq!(dispatcher.telemetry().native_value("gear"), Some(&LegacyValue::Int(-1)));
        assert_eq!(dispatcher.telemetry().vehicle, "Ginetta G56");
    }

    #[test]
    fn channels() {
        // Numeric projection of the integer rpm and integer overall speed, max_rpm 0 (no such field),
        // gear as text, abs(slip_ratio), spring_strain, acceleration_ls z/y/x and velocity_ls z/x/y rounded
        // to 6 decimals, quaternion angles with roll negated; missing wheels read 0.
        let mut t = Telemetry::off();
        write_channels(&connected_feed(), &mut t);
        assert_eq!((t.rpm, t.max_rpm, t.gear, t.speed), (6999.0, 0.0, -1.0, -33.0));
        assert_eq!(t.vehicle, "Ginetta G56");
        assert_eq!((t.wheel_slip_lf, t.wheel_slip_rf, t.wheel_slip_lr), (0.5, 0.25, 0.0));
        assert_eq!((t.suspension_travel_lf, t.suspension_travel_rf, t.suspension_travel_rr), (0.01, 0.02, 0.0));
        assert_eq!(
            (t.lateral_acceleration, t.vertical_acceleration, t.longitudinal_acceleration),
            (-9.876544, 0.2, 0.123456)
        );
        assert_eq!((t.lateral_velocity, t.longitudinal_velocity, t.vertical_velocity), (3.000001, 1.0, -2.5));
        assert_eq!(t.yaw, quaternion::yaw(0.1, 0.2, 0.3, 0.9));
        assert_eq!(t.roll, -quaternion::roll(0.1, 0.2, 0.3, 0.9));
        assert_eq!(t.pitch, quaternion::pitch(0.1, 0.2, 0.3, 0.9));
    }

    #[test]
    fn channels_gate_and_fallbacks() {
        // Channels read 0 while not connected or without player telemetry; car_name 0 for an empty name;
        // a NaN has no integer value, so rpm reads 0.
        let mut feed = connected_feed();
        feed.connected = false;
        let mut t = Telemetry::off();
        t.vehicle = "stale".into();
        write_channels(&feed, &mut t);
        assert_eq!((t.rpm, t.gear, t.yaw), (0.0, 0.0, 0.0));
        assert!(t.vehicle.is_empty());

        let mut feed = connected_feed();
        feed.player_name = Some(String::new());
        Arc::make_mut(feed.telemetry.as_mut().unwrap()).engine_rpm = f64::NAN;
        write_channels(&feed, &mut t);
        assert!(t.vehicle.is_empty());
        assert_eq!((t.rpm, t.speed), (0.0, -33.0));
    }

    #[test]
    fn the_loop_follows_game_running() {
        // The receive loop starts with the source and serves while the running game is one of GAME_IDS;
        // leaving the game ends the pass and closes the socket.
        let parameters = Arc::new(Parameters::default());
        let idle_net = FakeNetwork::default();
        let idle = PmRacingSource::with_parts(settings(&[]), Box::new(idle_net.clone()), parameters.clone());
        thread::sleep(Duration::from_millis(50));
        assert!(idle_net.bound().is_empty(), "game_running 'none': no socket");
        drop(idle);
        parameters.set_game_running("pmracing");
        let net = FakeNetwork::default();
        let source = PmRacingSource::with_parts(settings(&[]), Box::new(net.clone()), parameters.clone());
        let started = Instant::now();
        while net.bound().is_empty() {
            assert!(started.elapsed() < Duration::from_secs(3), "served without a liveness test");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(source.link.is_active());
        parameters.set_game_running("dirt4");
        assert!(!source.link.is_active(), "another game: the pass ends after the current receive");
    }

    #[test]
    fn source_reports_connected_and_shuts_its_loop_down() {
        // The liveness test and the receive step answer the connected flag; dropping the source ends the receive thread promptly
        // even while it sleeps after a failed socket.
        let net = FakeNetwork::default();
        net.refuse_bind(true);
        let mut source = PmRacingSource::with_network(settings(&[]), Box::new(net.clone()));
        assert!(!source.is_running().unwrap());
        assert_eq!(source.native_channel("gear"), Some(LegacyValue::Int(0)));
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(0)));
        let mut t = Telemetry::off();
        assert!(!source.poll(&mut t).unwrap());
        *source.link.feed() = connected_feed();
        assert!(source.is_running().unwrap());
        assert!(source.poll(&mut t).unwrap());
        assert_eq!(t.gear, -1.0);
        assert_eq!(source.native_channel("gear"), Some(LegacyValue::Str("-1".into())));
        let link = Arc::clone(&source.link);
        let started = Instant::now();
        drop(source);
        while Arc::strong_count(&link) > 1 {
            assert!(started.elapsed() < Duration::from_secs(1), "receive loop still running");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

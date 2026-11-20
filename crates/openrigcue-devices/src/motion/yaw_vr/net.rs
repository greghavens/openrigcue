//! Sockets of the Yaw VR driver: the TCP control channel and the two background loops that run as
//! threads (`call_devices` discovery and `read_response` feedback).

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tracing::debug;

use super::export::{GameEngineExport, GameEngineRecord};
use super::protocol::{self, parse_discovery_reply, parse_report, YawDevice, YawReport, CALLING, RECV_SIZE};
use super::{stop_platform, YawShared, YawVrSettings};
use crate::motion::clock::{Clock, SystemClock};
use crate::motion::{python, LiveSettings};

/// Pause between discovery rounds.
const DISCOVERY_PAUSE: Duration = Duration::from_secs(5);

/// The TCP control socket.
pub trait ControlChannel: Send {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// One receive of at most `max` bytes.
    fn recv(&mut self, max: usize) -> io::Result<Vec<u8>>;
}

/// Opens the control channel to an address (on a socket with a 7 s timeout).
pub type ControlConnector = Box<dyn FnMut(SocketAddr) -> io::Result<Box<dyn ControlChannel>> + Send>;

struct TcpControl(TcpStream);

impl ControlChannel for TcpControl {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    fn recv(&mut self, max: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0; max];
        let n = self.0.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

/// The real connector: TCP with a 7 s timeout for connect, send and receive.
pub fn tcp_connector() -> ControlConnector {
    Box::new(|addr| {
        let timeout = Duration::from_secs(protocol::SOCKET_TIMEOUT_SECS);
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Box::new(TcpControl(stream)) as Box<dyn ControlChannel>)
    })
}

/// A UDP socket with `SO_REUSEADDR` and `SO_BROADCAST`, the 7 s timeout only when two-way communication is
/// disabled, bound to `0.0.0.0:port`.
fn bound_udp(port: u16, two_way: bool) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    if !two_way {
        socket.set_read_timeout(Some(Duration::from_secs(protocol::SOCKET_TIMEOUT_SECS)))?;
    }
    socket.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(socket.into())
}

/// Stores `device` under `ip`: replace in place or append.
pub fn record_device(devices: &mut Vec<(String, YawDevice)>, ip: String, device: YawDevice) {
    match devices.iter_mut().find(|(k, _)| *k == ip) {
        Some((_, d)) => *d = device,
        None => devices.push((ip, device)),
    }
}

/// The socket of one discovery round: a new socket every round, bound to 22222; the 7 s timeout is set
/// unless two-way
/// communication is disabled at that moment. It closes when the round ends.
pub fn discovery_socket(settings: &YawVrSettings) -> io::Result<UdpSocket> {
    // The 7 s timeout applies only with two-way communication disabled; the switch is read every round.
    let two_way = settings.two_way_now().map_err(raised_io)?;
    bound_udp(protocol::DISCOVERY_BIND_PORT, two_way)
}

/// A driver error of a thread step as the I/O error that ends the thread.
fn raised_io(error: crate::motion::MotionError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

/// Records one discovery reply under its sender's address and logs it at debug level; a reply that
/// cannot be parsed is passed over silently.
fn record_reply(data: &[u8], from: SocketAddr, shared: &LiveSettings<YawShared>) {
    if let Some(device) = parse_discovery_reply(data) {
        let logged = device.python_str();
        shared.update(|s| record_device(&mut s.devices, from.ip().to_string(), device));
        debug!("yaw telemetry {logged}");
    }
}

/// One discovery round on its socket: broadcast `YAW_CALLING` (a failure ends the thread),
/// then record every reply until a receive fails (the 7 s timeout
/// ends the round). Without the timeout the receives block, so the round lasts while replies come.
pub fn discovery_round(socket: &UdpSocket, shared: &LiveSettings<YawShared>, stop: &AtomicBool) -> io::Result<()> {
    debug!("sending yaw calling");
    socket.send_to(CALLING, (Ipv4Addr::BROADCAST, protocol::DEVICE_UDP_PORT))?;
    receive_replies(socket, shared, stop);
    Ok(())
}

/// The inner loop of a discovery round: record replies until a receive fails.
fn receive_replies(socket: &UdpSocket, shared: &LiveSettings<YawShared>, stop: &AtomicBool) {
    let mut buf = vec![0; RECV_SIZE];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => record_reply(&buf[..n], from, shared),
            Err(e) => {
                debug!("didn t get any answer will call again {}", python::os_error_str(&e));
                break;
            }
        }
    }
}

/// What is written to stderr when a thread ends with an error: the thread's name,
/// then the error.
fn thread_died(name: &str, error: &io::Error) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "Exception in thread {name}:\n{error}");
}

/// The discovery loop: for ever, while a Yaw VR model is selected
/// (the model's manufacturer is `Yaw VR`; a failing lookup counts as not selected) a round on a new socket,
/// then a pause of five seconds in every case. A socket that cannot be set up or a failed broadcast ends the
/// thread.
fn call_devices(
    settings: &LiveSettings<YawVrSettings>,
    shared: &LiveSettings<YawShared>,
    yaw_vr_selected: &dyn Fn() -> bool,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        if yaw_vr_selected() {
            let round = discovery_socket(&settings.get()).and_then(|socket| discovery_round(&socket, shared, stop));
            if let Err(e) = round {
                thread_died("call_devices", &e);
                return;
            }
        }
        std::thread::sleep(DISCOVERY_PAUSE);
    }
}

/// Never set: the two threads run for the whole process.
static RUNS_FOR_THE_WHOLE_PROCESS: AtomicBool = AtomicBool::new(false);

/// Starts the `call_devices` thread: the discovery runs for the
/// whole process and tests whether a Yaw VR model is selected before each round. `Err` only when the thread
/// cannot be created.
pub fn start_call_devices(
    settings: LiveSettings<YawVrSettings>,
    shared: LiveSettings<YawShared>,
    yaw_vr_selected: Box<dyn Fn() -> bool + Send>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("call_devices".into())
        .spawn(move || call_devices(&settings, &shared, yaw_vr_selected.as_ref(), &RUNS_FOR_THE_WHOLE_PROCESS))
}

/// Motion-compensation values derived from a report: the raw yaw and the calculated yaw
/// (the yaw plus the `MOTION_COMPENSATION` yaw offset, negated when the yaw is reversed).
pub fn compensation_yaw(report: &YawReport, settings: &YawVrSettings) -> (f64, f64) {
    let factor = if settings.compensation_yaw_reverse { -1.0 } else { 1.0 };
    (report.yaw, (report.yaw + settings.compensation_yaw_offset) * factor)
}

fn native_compensation_yaw(report: &YawReport, settings: &YawVrSettings) -> Option<(f64, f64)> {
    let Some(native) = &settings.native_compensation else {
        return Some(compensation_yaw(report, settings));
    };
    let reverse = native.reverse?;
    let offset = native.offset?;
    Some((report.yaw, (report.yaw + offset) * if reverse { -1.0 } else { 1.0 }))
}

/// Handles one feedback datagram; `Err` for text that
/// is not ASCII. Returns the record the datagram leaves in the `YawVRGEFile` block, `None` when
/// it does not touch it ("Unexpected message").
pub fn handle_feedback(
    data: &[u8],
    settings: &YawVrSettings,
    shared: &LiveSettings<YawShared>,
) -> io::Result<Option<GameEngineRecord>> {
    let (record, result) = handle_feedback_partial(data, settings, shared);
    result.map(|()| record)
}

/// [`handle_feedback`] with the record the block holds even when a later step failed: the
/// `Yaw VR.6dof_mode` setting is read for each report after YAW…BATTERY were written, so its error
/// leaves those four with the previous SIXDOF.
pub fn handle_feedback_partial(
    data: &[u8],
    settings: &YawVrSettings,
    shared: &LiveSettings<YawShared>,
) -> (Option<GameEngineRecord>, io::Result<()>) {
    match handle_feedback_inner(data, settings, shared) {
        Ok((record, raised)) => (record, raised.map_or(Ok(()), Err)),
        Err(error) => (None, Err(error)),
    }
}

fn handle_feedback_inner(
    data: &[u8],
    settings: &YawVrSettings,
    shared: &LiveSettings<YawShared>,
) -> io::Result<(Option<GameEngineRecord>, Option<io::Error>)> {
    if !data.is_ascii() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "feedback is not ASCII"));
    }
    let text = String::from_utf8_lossy(data);
    match parse_report(&text) {
        Some(report) => {
            let result = native_compensation_yaw(&report, settings);
            let (raw, calculated) = result.unwrap_or((0.0, 0.0));
            let sixdof = settings.sixdof_now();
            let previous_sixdof = shared.read(|s| s.sixdof);
            shared.update(|s| {
                s.report = Some(report);
                s.compensation_zeroed = result.is_none();
                s.calculated_yaw = calculated;
                if let Some(values) = &s.sensor_values {
                    values.set(
                        "calculated_yaw",
                        if result.is_none() {
                            openrigcue_model::native_value::LegacyValue::Int(0)
                        } else {
                            openrigcue_model::native_value::LegacyValue::Float(calculated)
                        },
                    );
                }
                s.raw_yaw = raw;
                if let Some(values) = &s.sensor_values {
                    values.set(
                        "raw_yaw",
                        if result.is_none() {
                            openrigcue_model::native_value::LegacyValue::Int(0)
                        } else {
                            openrigcue_model::native_value::LegacyValue::Float(raw)
                        },
                    );
                }
                if let Ok(sixdof) = sixdof {
                    s.sixdof = sixdof;
                }
            });
            Ok(match sixdof {
                Ok(sixdof) => (Some(GameEngineRecord::from_report(&report, sixdof)), None),
                Err(error) => (Some(GameEngineRecord::from_report(&report, previous_sixdof)), Some(raised_io(error))),
            })
        }
        None => {
            debug!("Unexpected message: {text}");
            Ok((None, None))
        }
    }
}

/// A failed feedback read: when the motion status is
/// `'Connected'` and a Yaw VR model is selected, the driver disconnects at
/// once: the stop/park/exit commands on the control channel, the centring and start flags, then `'Not connected'`
/// and an empty motion port. Nothing else happens; nothing is logged.
pub fn feedback_failed(
    settings: &YawVrSettings,
    shared: &LiveSettings<YawShared>,
    yaw_vr_selected: &dyn Fn() -> bool,
    clock: &mut dyn Clock,
) -> Result<(), crate::motion::MotionError> {
    let link = shared.read(|s| s.link.clone());
    if !link.is_connected() || !yaw_vr_selected() {
        return Ok(());
    }
    stop_platform(settings, shared, clock)?;
    link.drop_link();
    Ok(())
}

/// The feedback socket, bound when the thread starts: `[Yaw VR] udp_port` on `0.0.0.0`, with the
/// 7 s timeout unless two-way communication is disabled.
fn read_response_socket(settings: &YawVrSettings) -> io::Result<UdpSocket> {
    // The port setting as an integer of any width, then the socket's range test.
    let port = settings.udp_port_value().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let port = u16::try_from(port)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "OverflowError: bind(): port must be 0-65535."))?;
    // Read once: the socket is set up when the thread starts.
    let two_way = settings.two_way_now().map_err(raised_io)?;
    bound_udp(port, two_way)
}

/// The feedback loop on its bound socket: a receive error (the 7 s timeout) or an
/// undecodable datagram runs [`feedback_failed`]; each report is also stored in `export` (the `YawVRGEFile`
/// block), when there is one.
fn read_response(
    socket: &UdpSocket,
    settings: &LiveSettings<YawVrSettings>,
    shared: &LiveSettings<YawShared>,
    export: Option<&GameEngineExport>,
    yaw_vr_selected: &dyn Fn() -> bool,
    stop: &AtomicBool,
) {
    let mut clock = SystemClock::default();
    let mut buf = vec![0; RECV_SIZE];
    while !stop.load(Ordering::Relaxed) {
        let result =
            socket.recv_from(&mut buf).map(|(n, _)| handle_feedback_partial(&buf[..n], &settings.get(), shared));
        let result = match result {
            Ok((record, result)) => {
                store_record(export, record);
                result
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {}
            Err(_) => {
                // An error of the disconnect inside the handler ends the thread.
                if let Err(raised) = feedback_failed(&settings.get(), shared, yaw_vr_selected, &mut clock) {
                    tracing::error!("Exception in thread read_response: {raised}");
                    return;
                }
            }
        }
    }
}

/// Starts the `read_response` thread: it
/// binds `[Yaw VR] udp_port` on its thread and reads it for the whole process. A bind that fails ends the
/// thread. `Err` only when the thread cannot be created.
pub fn start_read_response(
    settings: LiveSettings<YawVrSettings>,
    shared: LiveSettings<YawShared>,
    export: Option<Arc<GameEngineExport>>,
    yaw_vr_selected: Box<dyn Fn() -> bool + Send>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name("read_response".into()).spawn(move || {
        match read_response_socket(&settings.get()) {
            Ok(socket) => read_response(
                &socket,
                &settings,
                &shared,
                export.as_deref(),
                yaw_vr_selected.as_ref(),
                &RUNS_FOR_THE_WHOLE_PROCESS,
            ),
            Err(e) => thread_died("read_response", &e),
        }
    })
}

/// Stores the record of one report in the block. A failed file
/// write is only logged and does not count as a lost link.
pub fn store_record(export: Option<&GameEngineExport>, record: Option<GameEngineRecord>) {
    if let (Some(export), Some(record)) = (export, record) {
        if let Err(error) = export.write(&record) {
            debug!("cannot write {}: {error}", export.path().display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_feedback_scalar_failure_zeroes_rows_and_keeps_report_then_recovers() {
        use crate::motion::yaw_vr::NativeYawCompensation;
        use openrigcue_model::{motion_sensor::MotionSensorValues, native_value::LegacyValue};
        let shared = LiveSettings::new(YawShared::default());
        let values = MotionSensorValues::default();
        shared.update(|state| state.share_motion_sensor(values.clone()));
        let good = YawVrSettings {
            native_compensation: Some(NativeYawCompensation { reverse: Some(false), offset: Some(10.0) }),
            sixdof_mode: true,
            ..YawVrSettings::default()
        };
        handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &good, &shared).unwrap();
        for native in [
            NativeYawCompensation { reverse: None, offset: Some(10.0) },
            NativeYawCompensation { reverse: Some(true), offset: None },
        ] {
            let bad = YawVrSettings { native_compensation: Some(native), ..good.clone() };
            let record = handle_feedback(b"SY[25]SP[3]SR[4]U[80]", &bad, &shared).unwrap().unwrap();
            let report = YawReport { yaw: 25.0, pitch: 3.0, roll: 4.0, battery: 80.0 };
            assert_eq!(record, GameEngineRecord::from_report(&report, true));
            let state = shared.get();
            assert_eq!(state.report, Some(report));
            assert_eq!((state.raw_yaw, state.calculated_yaw, state.compensation_zeroed), (0.0, 0.0, true));
            for key in ["calculated_yaw", "raw_yaw"] {
                assert_eq!(values.get(key), Some(LegacyValue::Int(0)));
            }
            let late = MotionSensorValues::default();
            shared.update(|state| state.share_motion_sensor(late.clone()));
            assert_eq!(late.get("calculated_yaw"), Some(LegacyValue::Int(0)));
            assert_eq!(late.get("raw_yaw"), Some(LegacyValue::Int(0)));
            handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &good, &shared).unwrap();
            assert_eq!(late.get("calculated_yaw"), Some(LegacyValue::Float(30.0)));
            assert_eq!(late.get("raw_yaw"), Some(LegacyValue::Float(20.0)));
            assert!(!shared.get().compensation_zeroed);
        }
    }

    #[test]
    fn native_feedback_missing_scalars_do_not_change_unrecognized_or_ascii_failure_state() {
        use crate::motion::yaw_vr::NativeYawCompensation;
        let shared = LiveSettings::new(YawShared::default());
        let settings = YawVrSettings {
            native_compensation: Some(NativeYawCompensation { reverse: None, offset: None }),
            ..YawVrSettings::default()
        };
        assert_eq!(handle_feedback(b"Unexpected", &settings, &shared).unwrap(), None);
        assert_eq!(shared.get(), YawShared::default());
        assert!(handle_feedback(b"\xff", &settings, &shared).is_err());
        assert_eq!(shared.get(), YawShared::default());
        let nan = YawVrSettings {
            native_compensation: Some(NativeYawCompensation { reverse: Some(false), offset: Some(f64::NAN) }),
            ..settings
        };
        handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &nan, &shared).unwrap();
        assert!(
            shared.get().calculated_yaw.is_nan(),
            "accepted native float NaN must not become a caught conversion failure"
        );
        assert!(!shared.get().compensation_zeroed);
    }

    fn device(name: &str) -> YawDevice {
        YawDevice {
            model: "YAWDEVICE".into(),
            uuid: "u".into(),
            name: name.into(),
            tcp_port: "1".into(),
            status: "s".into(),
        }
    }

    /// Storing a device by sender address keeps insertion order and replaces in place.
    #[test]
    fn devices_keyed_by_sender_ip() {
        let mut d = Vec::new();
        record_device(&mut d, "10.0.0.2".into(), device("a"));
        record_device(&mut d, "10.0.0.3".into(), device("b"));
        record_device(&mut d, "10.0.0.2".into(), device("c"));
        assert_eq!(
            d.iter().map(|(k, v)| (k.as_str(), v.name.as_str())).collect::<Vec<_>>(),
            vec![("10.0.0.2", "c"), ("10.0.0.3", "b")]
        );
    }

    /// Discovery: every reply of a round is recorded (an unparsable one is passed over) until a receive
    /// fails, which ends the round.
    #[test]
    fn discovery_round_records_replies_until_a_receive_fails() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let target = socket.local_addr().unwrap();
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        a.send_to(b"YAWDEVICE;u1;Alpha;50020;READY", target).unwrap();
        b.send_to(b"\xff", target).unwrap();
        b.send_to(b"YAWDEVICE;u2;Beta;50020;READY", target).unwrap();
        let shared = LiveSettings::new(YawShared::default());
        let stop = AtomicBool::new(false);
        receive_replies(&socket, &shared, &stop);
        let devices = shared.get().devices;
        let names: Vec<&str> = devices.iter().map(|(_, d)| d.name.as_str()).collect();
        assert_eq!(names, ["Beta"], "all three come from 127.0.0.1: each good reply replaces the earlier one");
        assert_eq!(devices[0].0, "127.0.0.1");
    }

    /// Feedback: the raw yaw is YAW, the calculated yaw is YAW plus the offset, negated when reversed.
    #[test]
    fn feedback_updates_compensation_yaw() {
        let shared = LiveSettings::new(YawShared::default());
        let values = openrigcue_model::motion_sensor::MotionSensorValues::default();
        shared.update(|state| state.share_motion_sensor(values.clone()));
        let settings = YawVrSettings {
            compensation_yaw_offset: 10.0,
            compensation_yaw_reverse: true,
            sixdof_mode: true,
            ..YawVrSettings::default()
        };
        handle_feedback(b"SY[20]SP[1]SR[2]U[90]", &settings, &shared).unwrap();
        let s = shared.get();
        assert_eq!((s.raw_yaw, s.calculated_yaw, s.sixdof), (20.0, -30.0, true));
        assert_eq!(values.get("raw_yaw"), Some(openrigcue_model::native_value::LegacyValue::Float(20.0)));
        assert_eq!(values.get("calculated_yaw"), Some(openrigcue_model::native_value::LegacyValue::Float(-30.0)));
        assert_eq!(s.report.unwrap().battery, 90.0);
        assert!(handle_feedback("SY[\u{e9}]".as_bytes(), &settings, &shared).is_err());
        values.reset();
        assert_eq!(values.get("raw_yaw"), Some(openrigcue_model::native_value::LegacyValue::Int(0)));
        handle_feedback(b"SY[0]SP[0]SR[0]U[0]", &settings, &shared).unwrap();
        assert_eq!(values.get("raw_yaw"), Some(openrigcue_model::native_value::LegacyValue::Float(0.0)));
        let late = openrigcue_model::motion_sensor::MotionSensorValues::default();
        shared.update(|state| state.share_motion_sensor(late.clone()));
        assert_eq!(late.get("raw_yaw"), Some(openrigcue_model::native_value::LegacyValue::Float(0.0)));
        assert_eq!(late.get("calculated_yaw"), Some(openrigcue_model::native_value::LegacyValue::Float(-10.0)));
    }

    /// The feedback writes the block (YAW, PITCH, ROLL, BATTERY, then SIXDOF from `6dof_mode`) for every
    /// `SY` datagram; an unexpected one leaves the block as it was.
    #[test]
    fn feedback_fills_the_game_engine_block() {
        let dir = std::env::temp_dir().join(format!("openrigcue-yawvr-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let export = GameEngineExport::create(GameEngineExport::path_in(&dir)).unwrap();
        let shared = LiveSettings::new(YawShared::default());
        let settings = YawVrSettings { sixdof_mode: true, compensation_yaw_offset: 5.0, ..YawVrSettings::default() };

        let record = handle_feedback(b"SY[-12.5]SP[3.25]SR[x]U[77]", &settings, &shared).unwrap();
        let expected = GameEngineRecord { yaw: -12.5, pitch: 3.25, roll: 0.0, battery: 77.0, sixdof: true };
        assert_eq!(record, Some(expected), "the raw YAW, not calculated_yaw; 0 for a rejected field");
        store_record(Some(&export), record);
        assert_eq!(std::fs::read(export.path()).unwrap(), expected.to_bytes());

        let unexpected = handle_feedback(b"HELLO", &settings, &shared).unwrap();
        assert_eq!(unexpected, None);
        store_record(Some(&export), unexpected);
        assert_eq!(std::fs::read(export.path()).unwrap(), expected.to_bytes());

        export.remove().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The threads' switches are read where they are used: `disable_two_way_communication` when a
    /// discovery round sets up its socket, `6dof_mode` for each report after YAW…BATTERY (its error keeps the
    /// previous SIXDOF in the block and reaches the handler).
    #[test]
    fn native_thread_switches_are_read_at_their_statements() {
        use crate::motion::driver_support::testing::FakeConfig;
        use openrigcue_model::native_value::LegacyValue;
        let settings = YawVrSettings { sixdof_mode: true, ..YawVrSettings::default() };
        let config = std::sync::Arc::new(FakeConfig::default());
        settings.runtime.install_config(config.clone());
        config.set("Yaw VR", "disable_two_way_communication", LegacyValue::from("maybe"));
        let error = discovery_socket(&settings).unwrap_err();
        assert_eq!(error.to_string(), "Exception: Boolean value expected. Got maybe");
        config.set("Yaw VR", "disable_two_way_communication", LegacyValue::from("yes"));
        assert!(!settings.two_way_now().unwrap());
        let shared = LiveSettings::new(YawShared::default());
        config.set("Yaw VR", "6dof_mode", LegacyValue::from("no"));
        let (record, result) = handle_feedback_partial(b"SY[1.0]SP[2.0]SR[3.0]U[50]", &settings, &shared);
        result.unwrap();
        assert!(!record.unwrap().sixdof, "the live value, not the refreshed one");
        config.set("Yaw VR", "6dof_mode", LegacyValue::from("maybe"));
        let (record, result) = handle_feedback_partial(b"SY[4.0]SP[2.0]SR[3.0]U[50]", &settings, &shared);
        assert!(result.is_err());
        let record = record.unwrap();
        assert_eq!((record.yaw, record.sixdof), (4.0, false), "YAW written, SIXDOF kept");
        assert_eq!(shared.get().raw_yaw, 4.0);
    }
}

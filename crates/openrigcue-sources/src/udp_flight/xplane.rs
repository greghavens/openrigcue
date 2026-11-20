//! X-Plane: datarefs subscribed with the RREF protocol.
//!
//! While not connected, every liveness check opens a new socket and sends one RREF request per dataref to X-Plane's
//! own port; X-Plane then streams `"RREF"` + 1 byte + N × (i32 index, f32 value) to the request's source port.

use std::f64::consts::PI;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Mutex;
use std::time::Duration;

use openrigcue_model::{native_value::LegacyValue, Telemetry};

use super::channels::{assign, clear_listener_channels, AngularRates, FlightSource};
use super::python::{fold_roll, truncate};
use crate::udp_racing::socket::ImportedPort;
use crate::udp_racing::unhandled::DispatcherEnds;
use crate::{Source, SourceError};

/// Listener id.
pub const ID: &str = "xplane";
/// X-Plane's own receive port (`NETWORK_PORT.xplane`), the destination of the requests.
pub const DEFAULT_PORT: u16 = 49000;
const TIMEOUT: Duration = Duration::from_secs(1);
const MAX_DATAGRAM: usize = 1024;
/// Largest UDP payload.
const LARGEST_DATAGRAM: usize = 65_536;
/// Requested update rate of every dataref, in Hz.
const REQUEST_FREQUENCY: i32 = 25;
const REQUEST_LEN: usize = 413;
const PATH_LEN: usize = 400;
const HEADER: &[u8; 4] = b"RREF";
/// Bytes before the first (index, value) pair of a response.
const RESPONSE_HEADER_LEN: usize = 5;

/// Required rollback setting value, read only by the lateral-acceleration getter.
pub type NativeLateralReader = std::sync::Arc<dyn Fn() -> Option<LegacyValue> + Send + Sync>;
/// Engine rpm per rad/s: `(180 / π) / 6`.
const RAD_TO_RPM: f64 = 180.0 / PI / 6.0;
/// Initial value of the running maximum rpm.
const INITIAL_MAX_RPM: f64 = 1000.0;

/// Subscribed datarefs; the position is the RREF index.
const DATAREFS: [&str; 38] = [
    "sim/flightmodel/engine/ENGN_tacrad[0]",
    "sim/flightmodel/position/indicated_airspeed",
    "sim/flightmodel/position/local_ax",
    "sim/flightmodel/position/local_ay",
    "sim/flightmodel/forces/g_axil",
    "sim/flightmodel/position/theta",
    "sim/flightmodel/position/phi",
    "sim/flightmodel/position/psi",
    "sim/flightmodel/position/P",
    "sim/flightmodel/position/Q",
    "sim/flightmodel/position/R",
    "sim/aircraft/view/acf_descrip[0]",
    "sim/aircraft/view/acf_descrip[1]",
    "sim/aircraft/view/acf_descrip[2]",
    "sim/aircraft/view/acf_descrip[3]",
    "sim/aircraft/view/acf_descrip[4]",
    "sim/aircraft/view/acf_descrip[5]",
    "sim/aircraft/view/acf_descrip[6]",
    "sim/aircraft/view/acf_descrip[7]",
    "sim/aircraft/view/acf_descrip[8]",
    "sim/aircraft/view/acf_descrip[9]",
    "sim/aircraft/view/acf_descrip[10]",
    "sim/aircraft/view/acf_descrip[11]",
    "sim/aircraft/view/acf_descrip[12]",
    "sim/aircraft/view/acf_descrip[13]",
    "sim/aircraft/view/acf_descrip[14]",
    "sim/aircraft/view/acf_descrip[15]",
    "sim/aircraft/view/acf_descrip[16]",
    "sim/aircraft/view/acf_descrip[17]",
    "sim/aircraft/view/acf_descrip[18]",
    "sim/aircraft/view/acf_descrip[19]",
    "sim/flightmodel/engine/POINT_tacrad[0]",
    "sim/cockpit/warnings/annunciators/afterburners_on",
    "sim/flightmodel/controls/canopy_ratio",
    "sim/flightmodel2/gear/deploy_ratio[0]",
    "sim/flightmodel2/gear/deploy_ratio[1]",
    "sim/flightmodel2/gear/deploy_ratio[2]",
    "sim/flightmodel/forces/g_side",
];

/// RREF indices used by the channels.
mod index {
    pub const ENGINE_RAD_S: usize = 0;
    pub const AIRSPEED_KT: usize = 1;
    pub const LOCAL_AX: usize = 2;
    pub const LOCAL_AY: usize = 3;
    pub const G_AXIAL: usize = 4;
    pub const THETA: usize = 5;
    pub const PHI: usize = 6;
    pub const PSI: usize = 7;
    pub const P: usize = 8;
    pub const Q: usize = 9;
    pub const R: usize = 10;
    pub const NAME: std::ops::Range<usize> = 11..31;
    pub const PROPELLER_RAD_S: usize = 31;
    pub const AFTERBURNER: usize = 32;
    pub const CANOPY: usize = 33;
    pub const GEAR_LEFT: usize = 34;
    pub const GEAR_NOSE: usize = 35;
    pub const GEAR_RIGHT: usize = 36;
    pub const G_SIDE: usize = 37;
}

/// Listener options (`CUSTOM_FEATURES.xplane_invert_*`, `rollback_xplane_latg`; all `no` by default).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XPlaneOptions {
    pub invert_pitch: bool,
    pub invert_roll: bool,
    pub invert_yaw: bool,
    /// Take lateral acceleration from `local_ax` instead of `g_side`.
    pub lateral_g_from_local_ax: bool,
}

/// One RREF request: `RREF\0`, the rate, the index and the dataref path padded to 400 bytes.
fn request(index: usize, path: &str) -> [u8; REQUEST_LEN] {
    let mut message = [0u8; REQUEST_LEN];
    message[..HEADER.len()].copy_from_slice(HEADER);
    message[5..9].copy_from_slice(&REQUEST_FREQUENCY.to_le_bytes());
    message[9..13].copy_from_slice(&(index as i32).to_le_bytes());
    let path = path.as_bytes();
    let len = path.len().min(PATH_LEN);
    message[13..13 + len].copy_from_slice(&path[..len]);
    message
}

/// The character whose code is the truncated value; `None` outside `0..=0x10FFFF` or for a lone surrogate, which a
/// Rust string cannot hold.
fn python_chr(value: f64) -> Option<char> {
    let code = truncate(value)?;
    if !(0.0..=f64::from(u32::from(char::MAX))).contains(&code) {
        return None;
    }
    char::from_u32(code as u32)
}

/// Values of one response, by RREF index.
type Values = [Option<f32>; DATAREFS.len()];

trait XPlaneSocket: Send {
    fn set_timeout(&self, timeout: Duration) -> io::Result<()>;
    fn send(&self, bytes: &[u8], destination: SocketAddrV4) -> io::Result<()>;
    fn receive(&self, bytes: &mut [u8]) -> io::Result<usize>;
}

impl XPlaneSocket for UdpSocket {
    fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(timeout))
    }
    fn send(&self, bytes: &[u8], destination: SocketAddrV4) -> io::Result<()> {
        self.send_to(bytes, destination).map(|_| ())
    }
    fn receive(&self, bytes: &mut [u8]) -> io::Result<usize> {
        self.recv_from(bytes).map(|(size, _)| size)
    }
}

type SocketFactory = Box<dyn FnMut() -> io::Result<Box<dyn XPlaneSocket>> + Send>;

/// Decodes a response: a non-RREF datagram yields no values; an index outside the dataref
/// list (`-38..=37`) is an error. Negative indices are accepted and never read.
fn decode_response(datagram: &[u8]) -> Result<Values, ()> {
    let mut values: Values = [None; DATAREFS.len()];
    if datagram.get(..HEADER.len()) != Some(HEADER.as_slice()) {
        // An unknown packet is printed with its first four bytes and its data.
        let header = &datagram[..datagram.len().min(HEADER.len())];
        crate::printed::line(format!(
            "Unknown packet:  {} {}",
            crate::printed::bytes_repr(header),
            crate::printed::bytes_repr(datagram)
        ));
        return Ok(values);
    }
    let pairs = datagram.get(RESPONSE_HEADER_LEN..).unwrap_or_default().as_chunks::<8>().0;
    for pair in pairs {
        let index = i32::from_le_bytes(pair[..4].try_into().expect("4 bytes"));
        let value = f32::from_le_bytes(pair[4..].try_into().expect("4 bytes"));
        match usize::try_from(index) {
            Ok(i) if i < DATAREFS.len() => values[i] = Some(value),
            Err(_) if index >= -(DATAREFS.len() as i32) => {}
            _ => return Err(()),
        }
    }
    Ok(values)
}

/// The X-Plane listener (`xplane`).
pub struct XPlaneSource {
    destination: SocketAddrV4,
    imported_port: Option<ImportedPort>,
    options: XPlaneOptions,
    lateral_reader: Option<NativeLateralReader>,
    socket: Option<Box<dyn XPlaneSocket>>,
    create_socket: SocketFactory,
    connected: bool,
    datagram: Option<Vec<u8>>,
    values: Values,
    /// Highest rpm read so far.
    max_rpm: Mutex<f64>,
    rates: AngularRates,
}

impl XPlaneSource {
    /// `port` is X-Plane's receive port; requests go to `127.0.0.1:port`.
    pub fn new(port: u16, options: XPlaneOptions) -> Self {
        XPlaneSource {
            destination: SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            imported_port: None,
            options,
            lateral_reader: None,
            socket: None,
            create_socket: Box::new(|| {
                UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map(|socket| Box::new(socket) as Box<dyn XPlaneSocket>)
            }),
            connected: false,
            datagram: None,
            values: [None; DATAREFS.len()],
            max_rpm: Mutex::new(INITIAL_MAX_RPM),
            rates: AngularRates::default(),
        }
    }

    /// Defer range checking of the cached destination port until the first send.
    pub fn from_imported_port(port: ImportedPort, options: XPlaneOptions) -> Self {
        let mut source = Self::new(0, options);
        source.imported_port = Some(port);
        source.create_socket = Box::new(|| {
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
                .map(|socket| Box::new(UdpSocket::from(socket)) as Box<dyn XPlaneSocket>)
        });
        source
    }

    /// Changes the numeric compatibility source. Typed lateral reads use their required setting;
    /// axis inversions are read once at start-up (hence no setter for them).
    pub fn set_lateral_g_from_local_ax(&mut self, enabled: bool) {
        self.options.lateral_g_from_local_ax = enabled;
    }

    pub fn set_lateral_reader(&mut self, reader: NativeLateralReader) {
        self.lateral_reader = Some(reader);
    }

    fn native_lateral_from_local_ax(&self) -> Option<bool> {
        let Some(reader) = &self.lateral_reader else { return Some(self.options.lateral_g_from_local_ax) };
        match reader()? {
            LegacyValue::Bool(value) => Some(value),
            value => openrigcue_model::native_value::str2bool(
                &openrigcue_model::python_repr::str_native(&value).ok()?.scalar_string()?,
            ),
        }
    }

    /// Opens a new socket and sends the 38 requests (the not-connected branch of
    /// the liveness test). `Ok(false)`: the socket could not be made (the failure is printed
    /// and the listener stays not connected). A failed send is not handled: the error
    /// fails the liveness test and ends the dispatcher's loop.
    fn subscribe(&mut self) -> Result<bool, DispatcherEnds> {
        let socket = match (self.create_socket)() {
            Ok(socket) => socket,
            Err(error) => {
                crate::printed::line(format!("sock {}", crate::printed::os_error(&error)));
                self.connected = false;
                return Ok(false);
            }
        };
        let socket = self.socket.insert(socket);
        if let Err(error) = socket.set_timeout(TIMEOUT) {
            crate::printed::line(format!("sock {}", crate::printed::os_error(&error)));
            self.connected = false;
            return Ok(false);
        }
        for (index, path) in DATAREFS.iter().enumerate() {
            let message = request(index, path);
            let mut destination = self.destination;
            if let Some(port) = self.imported_port {
                let port = match port {
                    ImportedPort::Valid(port) => port,
                    ImportedPort::OutOfRange => return Err(DispatcherEnds::new("sendto(): port must be 0-65535.")),
                };
                destination.set_port(port);
            }
            socket
                .send(&message, destination)
                .map_err(|error| DispatcherEnds::new(format!("xplane RequestDataRefs: {error}")))?;
        }
        Ok(true)
    }

    fn value(&self, index: usize) -> Option<f64> {
        self.values[index].map(f64::from)
    }

    fn factor(invert: bool) -> f64 {
        if invert {
            -1.0
        } else {
            1.0
        }
    }

    fn rpm_value(&self) -> Option<f64> {
        let engine = self.value(index::ENGINE_RAD_S)?;
        let rad_s = if engine > 0.0 { engine } else { self.value(index::PROPELLER_RAD_S)? };
        Some(truncate(rad_s * RAD_TO_RPM)?.max(0.0))
    }

    /// The `rpm` channel: engine speed, or propeller speed when the engine reads
    /// 0 or less, clamped at 0; raises the running maximum.
    fn read_rpm(&self) -> Option<f64> {
        let rpm = self.rpm_value()?;
        let mut maximum = self.max_rpm.lock().unwrap_or_else(|e| e.into_inner());
        if rpm > *maximum {
            *maximum = rpm;
        }
        Some(rpm)
    }

    /// The `car_name` channel: one character code per `acf_descrip` element, trailing NULs removed.
    fn aircraft_name(&self) -> Option<String> {
        let name: String = index::NAME.map(|i| python_chr(self.value(i)?)).collect::<Option<String>>()?;
        Some(name.trim_end_matches('\0').to_owned())
    }

    /// Numeric compatibility replay (`max_rpm` before `rpm`). Typed dispatch makes its actual getter calls.
    fn write(&mut self, t: &mut Telemetry) {
        let (pitch_factor, roll_factor, yaw_factor) = (
            Self::factor(self.options.invert_pitch),
            Self::factor(self.options.invert_roll),
            Self::factor(self.options.invert_yaw),
        );
        let lateral_index = if self.options.lateral_g_from_local_ax { index::LOCAL_AX } else { index::G_SIDE };
        t.max_rpm = *self.max_rpm.lock().unwrap_or_else(|e| e.into_inner());
        assign(&mut t.rpm, self.read_rpm());
        assign(&mut t.speed, self.value(index::AIRSPEED_KT).and_then(truncate));
        assign(&mut t.steering_wheel_angle, self.value(index::LOCAL_AX));
        assign(&mut t.lateral_acceleration, self.value(lateral_index));
        assign(&mut t.vertical_acceleration, self.value(index::LOCAL_AY));
        assign(&mut t.longitudinal_acceleration, self.value(index::G_AXIAL).map(|g| g * -10.0));
        assign(&mut t.pitch, self.value(index::THETA).map(|v| v * pitch_factor));
        assign(&mut t.roll, self.value(index::PHI).map(|v| fold_roll(v * roll_factor)));
        assign(&mut t.yaw, self.value(index::PSI).map(|v| v * yaw_factor));
        assign(&mut t.lateral_velocity, self.value(index::Q));
        assign(&mut t.canopy, self.value(index::CANOPY));
        assign(&mut t.afterburner, self.value(index::AFTERBURNER).map(|v| if v > 0.0 { 1.0 } else { 0.0 }));
        assign(&mut t.landing_gear_nose, self.value(index::GEAR_NOSE));
        assign(&mut t.landing_gear_left, self.value(index::GEAR_LEFT));
        assign(&mut t.landing_gear_right, self.value(index::GEAR_RIGHT));
        if let Some(name) = self.aircraft_name() {
            t.vehicle = name;
        }
        self.rates = AngularRates {
            pitch: self.value(index::Q).map_or(0.0, |v| v * pitch_factor),
            roll: self.value(index::P).map_or(0.0, |v| v * roll_factor),
            yaw: self.value(index::R).map_or(0.0, |v| v * yaw_factor),
        };
    }
}

impl Source for XPlaneSource {
    fn id(&self) -> &'static str {
        ID
    }

    fn native_channel(&self, name: &str) -> Option<LegacyValue> {
        use LegacyValue::{Float, Int, Str};

        // Keep the existing bounded model policy for integer magnitudes beyond i64.
        let integer = |value: f64| {
            if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value) {
                Int(value as i64)
            } else {
                Float(value)
            }
        };
        let pitch_factor = Self::factor(self.options.invert_pitch);
        let roll_factor = Self::factor(self.options.invert_roll);
        let yaw_factor = Self::factor(self.options.invert_yaw);
        let value = match name {
            "rpm" => return Some(self.read_rpm().map_or(Int(0), integer)),
            "max_rpm" => return Some(integer(*self.max_rpm.lock().unwrap_or_else(|e| e.into_inner()))),
            "gear" => return Some(Str("0".into())),
            "speed" => {
                return Some(Str(self
                    .value(index::AIRSPEED_KT)
                    .and_then(crate::udp_racing::int_text)
                    .unwrap_or_else(|| "0".into())));
            }
            "car_name" => return Some(self.aircraft_name().map_or(Int(0), Str)),
            "afterburner" => return Some(Int(i64::from(self.value(index::AFTERBURNER).is_some_and(|v| v > 0.0)))),
            "steering_wheel_angle" => self.value(index::LOCAL_AX),
            "lateral_acceleration" => {
                self.value(if self.native_lateral_from_local_ax()? { index::LOCAL_AX } else { index::G_SIDE })
            }
            "vertical_acceleration" => self.value(index::LOCAL_AY),
            "longitudinal_acceleration" => self.value(index::G_AXIAL).map(|g| g * -10.0),
            "pitch" => self.value(index::THETA).map(|v| v * pitch_factor),
            "roll" => self.value(index::PHI).map(|v| fold_roll(v * roll_factor)),
            "yaw" => self.value(index::PSI).map(|v| v * yaw_factor),
            "pitch_rate" => self.value(index::Q).map(|v| v * pitch_factor),
            "roll_rate" => self.value(index::P).map(|v| v * roll_factor),
            "yaw_rate" => self.value(index::R).map(|v| v * yaw_factor),
            "lateral_velocity" => self.value(index::Q),
            "canopy" => self.value(index::CANOPY),
            "landing_gear_nose" => self.value(index::GEAR_NOSE),
            "landing_gear_left" => self.value(index::GEAR_LEFT),
            "landing_gear_right" => self.value(index::GEAR_RIGHT),
            _ => None,
        };
        Some(value.map_or(Int(0), Float))
    }

    fn read_native(&self, name: &str) -> crate::NativeRead {
        self.native_channel(name).map_or(crate::NativeRead::Raised, crate::NativeRead::Value)
    }

    /// The liveness test.
    fn is_running(&mut self) -> Result<bool, SourceError> {
        if !self.connected && !self.subscribe()? {
            return Ok(false);
        }
        // Room for any UDP payload: a datagram longer than 1024 bytes is a receive error,
        // treated like a timeout, not a truncation.
        let mut buf = vec![0; LARGEST_DATAGRAM];
        let received = self.socket.as_ref().map(|socket| socket.receive(&mut buf));
        match received {
            Some(Ok(len)) if len <= MAX_DATAGRAM => {
                buf.truncate(len);
                self.datagram = Some(buf);
                self.connected = true;
            }
            _ => self.connected = false,
        }
        Ok(self.connected)
    }

    /// The receive step, without invoking getters or changing the running maximum.
    fn receive_native(&mut self, _telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let decoded = match &self.datagram {
            Some(datagram) => decode_response(datagram),
            None => Err(()),
        };
        Ok(match decoded {
            Ok(values) => {
                self.values = values;
                true
            }
            Err(()) => {
                if self.datagram.is_some() {
                    tracing::warn!("xplane error while decoding packet");
                    self.connected = false;
                }
                false
            }
        })
    }

    /// Numeric compatibility receive/getter replay.
    fn poll(&mut self, telemetry: &mut Telemetry) -> Result<bool, SourceError> {
        let fresh = self.receive_native(telemetry)?;
        clear_listener_channels(telemetry);
        self.write(telemetry);
        Ok(fresh)
    }
}

impl FlightSource for XPlaneSource {
    fn angular_rates(&self) -> AngularRates {
        self.rates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    struct FakeSocket {
        id: usize,
        calls: Arc<Mutex<Vec<String>>>,
        timeout_error: bool,
        sends: Mutex<usize>,
        send_error_at: Option<usize>,
        received: Mutex<VecDeque<io::Result<Vec<u8>>>>,
    }

    impl XPlaneSocket for FakeSocket {
        fn set_timeout(&self, timeout: Duration) -> io::Result<()> {
            assert_eq!(timeout, Duration::from_secs(1));
            self.calls.lock().unwrap().push(format!("timeout:{}", self.id));
            if self.timeout_error {
                Err(io::Error::other("timeout rejected"))
            } else {
                Ok(())
            }
        }
        fn send(&self, bytes: &[u8], destination: SocketAddrV4) -> io::Result<()> {
            assert_eq!(destination, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 6001));
            let mut sends = self.sends.lock().unwrap();
            assert_eq!(bytes, request(*sends, DATAREFS[*sends]));
            self.calls.lock().unwrap().push(format!("send:{}:{}", self.id, *sends));
            let failed = self.send_error_at == Some(*sends);
            *sends += 1;
            if failed {
                Err(io::Error::other("send rejected"))
            } else {
                Ok(())
            }
        }
        fn receive(&self, bytes: &mut [u8]) -> io::Result<usize> {
            self.calls.lock().unwrap().push(format!("receive:{}", self.id));
            let data = self.received.lock().unwrap().pop_front().expect("scripted receive")?;
            bytes[..data.len()].copy_from_slice(&data);
            Ok(data.len())
        }
    }
    impl Drop for FakeSocket {
        fn drop(&mut self) {
            self.calls.lock().unwrap().push(format!("drop:{}", self.id));
        }
    }

    fn fake_socket(
        id: usize,
        calls: &Arc<Mutex<Vec<String>>>,
        timeout_error: bool,
        send_error_at: Option<usize>,
        received: Vec<io::Result<Vec<u8>>>,
    ) -> Box<dyn XPlaneSocket> {
        Box::new(FakeSocket {
            id,
            calls: calls.clone(),
            timeout_error,
            sends: Mutex::new(0),
            send_error_at,
            received: Mutex::new(received.into()),
        })
    }

    #[test]
    fn native_destination_is_checked_after_creation_assignment_and_timeout() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = XPlaneSource::from_imported_port(ImportedPort::OutOfRange, XPlaneOptions::default());
        source.socket = Some(fake_socket(0, &calls, false, None, vec![]));
        let log = calls.clone();
        let mut failures = VecDeque::from([true, false, false]);
        let mut id = 0;
        source.create_socket = Box::new(move || {
            log.lock().unwrap().push("create".into());
            if failures.pop_front().unwrap() {
                return Err(io::Error::other("creation rejected"));
            }
            id += 1;
            Ok(fake_socket(id, &log, id == 1, None, vec![]))
        });
        assert!(!source.is_running().unwrap());
        assert!(source.socket.is_some(), "creation failure preserves the previous socket");
        assert_eq!(*calls.lock().unwrap(), ["create"]);
        assert!(!source.is_running().unwrap());
        assert_eq!(*calls.lock().unwrap(), ["create", "create", "drop:0", "timeout:1"]);
        assert!(source.socket.is_some(), "timeout failure retains the newly assigned socket");
        let error = source.is_running().unwrap_err();
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error));
        assert!(error.to_string().contains("sendto(): port must be 0-65535."));
        assert_eq!(
            *calls.lock().unwrap(),
            ["create", "create", "drop:0", "timeout:1", "create", "drop:1", "timeout:2"]
        );
        assert!(!source.connected);
    }

    #[test]
    fn native_subscription_send_error_retains_completed_request_prefix() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = XPlaneSource::from_imported_port(ImportedPort::Valid(6001), XPlaneOptions::default());
        let log = calls.clone();
        source.create_socket = Box::new(move || Ok(fake_socket(1, &log, false, Some(2), vec![])));
        let error = source.is_running().unwrap_err();
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error));
        assert_eq!(*calls.lock().unwrap(), ["timeout:1", "send:1:0", "send:1:1", "send:1:2"]);
        assert!(source.socket.is_some());
        assert!(!source.connected);
    }

    #[test]
    fn native_subscription_reuses_connected_socket_then_retries_after_receive_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut source = XPlaneSource::from_imported_port(ImportedPort::Valid(6001), XPlaneOptions::default());
        let log = calls.clone();
        let mut id = 0;
        source.create_socket = Box::new(move || {
            id += 1;
            log.lock().unwrap().push("create".into());
            Ok(fake_socket(
                id,
                &log,
                false,
                None,
                if id == 1 {
                    vec![Ok(response(&[(0, 200.0)])), Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))]
                } else {
                    vec![Ok(response(&[(0, 300.0)]))]
                },
            ))
        });
        assert!(source.is_running().unwrap());
        assert_eq!(calls.lock().unwrap().iter().filter(|x| x.starts_with("send:")).count(), 38);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        let old = source.native_channel("rpm");
        assert!(!source.is_running().unwrap());
        assert_eq!(source.native_channel("rpm"), old);
        assert_eq!(calls.lock().unwrap().iter().filter(|x| x.starts_with("send:")).count(), 38);
        assert!(source.is_running().unwrap());
        assert_eq!(calls.lock().unwrap().iter().filter(|x| x.starts_with("send:")).count(), 76);
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_ne!(source.native_channel("rpm"), old);
    }

    fn response(pairs: &[(i32, f32)]) -> Vec<u8> {
        let mut bytes = b"RREF,".to_vec();
        for (index, value) in pairs {
            bytes.extend_from_slice(&index.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn source_with(pairs: &[(i32, f32)], options: XPlaneOptions) -> (XPlaneSource, Telemetry) {
        let mut source = XPlaneSource::new(DEFAULT_PORT, options);
        source.datagram = Some(response(pairs));
        let mut t = Telemetry::off();
        assert!(source.poll(&mut t).unwrap());
        (source, t)
    }

    #[test]
    fn native_lateral_reads_the_current_required_setting() {
        use crate::{
            derive::{Listener, SourceListener},
            NativeRead,
        };
        let current = Arc::new(Mutex::new(Some(LegacyValue::from("yes"))));
        let mut source = XPlaneSource::new(DEFAULT_PORT, XPlaneOptions::default());
        source.values = decode_response(&response(&[(2, 3.0), (37, 7.0)])).unwrap();
        let read = current.clone();
        source.set_lateral_reader(Arc::new(move || read.lock().unwrap().clone()));
        let listener = SourceListener::new(source);
        assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Float(3.0)));
        *current.lock().unwrap() = Some(LegacyValue::from("no"));
        assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Float(7.0)));
        *current.lock().unwrap() = Some(LegacyValue::from("yes"));
        assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Float(3.0)));
    }

    #[test]
    fn native_lateral_required_failure_precedes_the_data_fallback() {
        use crate::{
            derive::{Listener, SourceListener},
            NativeRead,
        };
        let current = Arc::new(Mutex::new(None));
        let mut source = XPlaneSource::new(DEFAULT_PORT, XPlaneOptions::default());
        let read = current.clone();
        source.set_lateral_reader(Arc::new(move || read.lock().unwrap().clone()));
        let listener = SourceListener::new(source);
        assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Raised);
        for value in [LegacyValue::None, LegacyValue::from("invalid"), LegacyValue::Int(2)] {
            *current.lock().unwrap() = Some(value);
            assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Raised);
        }
        *current.lock().unwrap() = Some(LegacyValue::from("no"));
        assert_eq!(listener.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Int(0)));
    }

    #[test]
    fn native_lateral_scalar_conversion_is_local_and_read_once() {
        use crate::NativeRead;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let current = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut source = XPlaneSource::new(DEFAULT_PORT, XPlaneOptions::default());
        source.values = decode_response(&response(&[(2, 3.0), (37, 7.0), (1, 42.0)])).unwrap();
        let read = current.clone();
        let counted = calls.clone();
        source.set_lateral_reader(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            read.lock().unwrap().clone()
        }));
        assert_eq!(source.read_native("speed"), NativeRead::Value(LegacyValue::from("42")));
        assert_eq!(source.read_native("rpm"), NativeRead::Value(LegacyValue::Int(0)));
        assert_eq!(source.read_native("steering_wheel_angle"), NativeRead::Value(LegacyValue::Float(3.0)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for (value, expected) in [
            (LegacyValue::from("Y"), Some(3.0)),
            (LegacyValue::Bool(true), Some(3.0)),
            (LegacyValue::Int(1), Some(3.0)),
            (LegacyValue::from("false"), Some(7.0)),
            (LegacyValue::Bool(false), Some(7.0)),
            (LegacyValue::Int(0), Some(7.0)),
            (LegacyValue::from(" yes "), None),
            (LegacyValue::Float(1.0), None),
            (LegacyValue::Bytes(b"yes".to_vec()), None),
            (LegacyValue::None, None),
        ] {
            *current.lock().unwrap() = Some(value);
            let before = calls.load(Ordering::SeqCst);
            assert_eq!(
                source.read_native("lateral_acceleration"),
                expected.map_or(NativeRead::Raised, |v| NativeRead::Value(LegacyValue::Float(v)))
            );
            assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        }
        *current.lock().unwrap() = Some(LegacyValue::from("yes"));
        source.values[index::LOCAL_AX] = None;
        assert_eq!(source.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Int(0)));
        *current.lock().unwrap() = Some(LegacyValue::from("no"));
        assert_eq!(source.read_native("lateral_acceleration"), NativeRead::Value(LegacyValue::Float(7.0)));
    }

    #[test]
    fn native_rpm_getter_updates_the_running_maximum_immediately() {
        let mut source = XPlaneSource::new(DEFAULT_PORT, XPlaneOptions::default());
        source.values = decode_response(&response(&[(0, 200.0)])).unwrap();
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(1000)));
        let rpm = (200.0 * RAD_TO_RPM).trunc() as i64;
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(rpm)));
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(rpm)));
    }

    #[test]
    fn native_receive_and_unrelated_getters_do_not_consume_rpm_values() {
        let mut source = XPlaneSource::new(DEFAULT_PORT, XPlaneOptions::default());
        source.datagram = Some(response(&[(0, 400.0), (1, 99.0), (9, 2.0)]));
        assert!(source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(1000)));
        assert_eq!(source.native_channel("speed"), Some(LegacyValue::Str("99".into())));
        assert_eq!(source.native_channel("pitch_rate"), Some(LegacyValue::Float(2.0)));
        source.datagram = Some(response(&[(0, 200.0)]));
        source.receive_native(&mut Telemetry::off()).unwrap();
        let rpm = (200.0 * RAD_TO_RPM).trunc() as i64;
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(rpm)));
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(rpm)));
        source.datagram = Some(response(&[(0, f32::INFINITY)]));
        source.receive_native(&mut Telemetry::off()).unwrap();
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(0)));
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(rpm)));
        source.datagram = Some(response(&[(0, 300.0), (38, 1.0)]));
        assert!(!source.receive_native(&mut Telemetry::off()).unwrap());
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(0)), "failed decode retained the prior values");
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(rpm)));
    }

    #[test]
    fn source_adapter_preserves_the_actual_maximum_and_rpm_read_order() {
        use crate::derive::{Listener, Rate, SourceListener};
        use crate::NativeRead;
        let mut source = XPlaneSource::new(DEFAULT_PORT, XPlaneOptions::default());
        source.datagram = Some(response(&[(0, 200.0), (9, 2.0)]));
        let mut listener = SourceListener::new(source);
        listener.receive();
        assert_eq!(listener.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(1000)));
        assert_eq!(listener.rate(Rate::Pitch), Some(2.0));
        assert_eq!(listener.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(1000)));
        let rpm = (200.0 * RAD_TO_RPM).trunc() as i64;
        assert_eq!(listener.read_native("rpm"), NativeRead::Value(LegacyValue::Int(rpm)));
        assert_eq!(listener.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(rpm)));
        listener.receive();
        assert_eq!(listener.read_native("max_rpm"), NativeRead::Value(LegacyValue::Int(rpm)));
    }

    #[test]
    fn request_data_refs_message_layout() {
        let message = request(7, DATAREFS[7]);
        assert_eq!(message.len(), 413);
        assert_eq!(&message[..5], b"RREF\0");
        assert_eq!(i32::from_le_bytes(message[5..9].try_into().unwrap()), 25);
        assert_eq!(i32::from_le_bytes(message[9..13].try_into().unwrap()), 7);
        assert_eq!(&message[13..13 + DATAREFS[7].len()], DATAREFS[7].as_bytes());
        assert!(message[13 + DATAREFS[7].len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn get_channel_maps_datarefs() {
        let (source, t) = source_with(
            &[
                (1, 120.7),
                (2, 0.5),
                (3, 9.5),
                (4, -0.25),
                (5, 4.0),
                (6, 100.0),
                (7, 270.0),
                (8, 1.0),
                (9, 2.0),
                (10, 3.0),
                (33, 0.5),
                (32, 1.0),
                (34, 0.1),
                (35, 0.2),
                (36, 0.3),
                (37, 0.75),
            ],
            XPlaneOptions::default(),
        );
        assert_eq!(t.speed, 120.0, "int(knots)");
        assert_eq!(t.steering_wheel_angle, 0.5);
        assert_eq!(t.lateral_acceleration, 0.75, "g_side by default");
        assert_eq!(t.vertical_acceleration, 9.5);
        assert_eq!(t.longitudinal_acceleration, 2.5);
        assert_eq!((t.pitch, t.roll, t.yaw), (4.0, 80.0, 270.0));
        assert_eq!(t.lateral_velocity, 2.0, "Q, as compiled");
        assert_eq!((t.canopy, t.afterburner), (0.5, 1.0));
        assert_eq!(
            (t.landing_gear_left, t.landing_gear_nose, t.landing_gear_right),
            (f64::from(0.1f32), f64::from(0.2f32), f64::from(0.3f32))
        );
        assert_eq!(t.gear, 0.0);
        assert_eq!(source.angular_rates(), AngularRates { pitch: 2.0, roll: 1.0, yaw: 3.0 });
        use LegacyValue::{Float, Int, Str};
        for (name, expected) in [
            ("rpm", Int(0)),
            ("max_rpm", Int(1000)),
            ("gear", Str("0".into())),
            ("speed", Str("120".into())),
            ("afterburner", Int(1)),
            ("pitch", Float(4.0)),
            ("roll", Float(80.0)),
            ("yaw_rate", Float(3.0)),
            ("vertical_velocity", Int(0)),
            ("car_name", Int(0)),
        ] {
            assert_eq!(source.native_channel(name), Some(expected), "{name}");
        }
    }

    #[test]
    fn get_channel_inverts_axes_and_rolls_back_lateral_g() {
        let options =
            XPlaneOptions { invert_pitch: true, invert_roll: true, invert_yaw: true, lateral_g_from_local_ax: true };
        let (source, t) = source_with(
            &[(2, 0.5), (5, 4.0), (6, -100.0), (7, 10.0), (8, 1.0), (9, 2.0), (10, 3.0), (37, 0.75)],
            options,
        );
        assert_eq!((t.pitch, t.roll, t.yaw), (-4.0, 80.0, -10.0));
        assert_eq!(t.lateral_acceleration, 0.5);
        assert_eq!(t.lateral_velocity, 2.0, "lateral_velocity has no factor");
        assert_eq!(source.angular_rates(), AngularRates { pitch: -2.0, roll: -1.0, yaw: -3.0 });
    }

    #[test]
    fn get_channel_rpm_falls_back_to_propeller_and_tracks_the_maximum() {
        let (mut source, t) = source_with(&[(0, 200.0)], XPlaneOptions::default());
        assert_eq!(t.rpm, (200.0 * RAD_TO_RPM).trunc());
        assert_eq!(t.max_rpm, 1000.0, "max_rpm is read before rpm raises it");
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(t.rpm as i64)));
        assert_eq!(source.native_channel("rpm"), Some(LegacyValue::Int(t.rpm as i64)));
        source.datagram = Some(response(&[(0, 0.0), (31, 50.0)]));
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!(t.max_rpm, (200.0 * RAD_TO_RPM).trunc());
        assert_eq!(t.rpm, (50.0 * RAD_TO_RPM).trunc());
        assert_eq!(source.native_channel("max_rpm"), Some(LegacyValue::Int(t.max_rpm as i64)));
        source.datagram = Some(response(&[(0, -5.0), (31, -5.0)]));
        source.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 0.0, "negative rpm clamps to 0");
        source.datagram = Some(response(&[(31, 50.0)]));
        source.poll(&mut t).unwrap();
        assert_eq!(t.rpm, 0.0, "missing engine value answers 0");
    }

    #[test]
    fn get_channel_car_name_from_character_codes() {
        let mut pairs: Vec<(i32, f32)> =
            "C172".bytes().enumerate().map(|(i, b)| (11 + i as i32, f32::from(b))).collect();
        pairs.extend((15..31).map(|i| (i, 0.0)));
        let (_, t) = source_with(&pairs, XPlaneOptions::default());
        assert_eq!(t.vehicle, "C172");
        pairs.pop();
        let (_, t) = source_with(&pairs, XPlaneOptions::default());
        assert!(t.vehicle.is_empty(), "a missing element answers 0");
    }

    #[test]
    fn decode_packet_unknown_header_clears_values() {
        let (mut source, _) = source_with(&[(5, 4.0)], XPlaneOptions::default());
        source.datagram = Some(b"DATA*....".to_vec());
        let mut t = Telemetry::off();
        crate::printed::take();
        assert!(source.poll(&mut t).unwrap());
        assert_eq!(t.pitch, 0.0);
        let printed = crate::printed::take();
        assert!(!printed.is_empty());
        assert!(printed.iter().all(|line| line == "Unknown packet:  b'DATA' b'DATA*....'"), "{printed:?}");
    }

    #[test]
    fn decode_packet_index_out_of_range_disconnects_and_keeps_values() {
        let (mut source, _) = source_with(&[(5, 4.0)], XPlaneOptions::default());
        source.connected = true;
        source.datagram = Some(response(&[(5, 9.0), (38, 1.0)]));
        let mut t = Telemetry::off();
        assert!(!source.poll(&mut t).unwrap());
        assert!(!source.connected);
        assert_eq!(t.pitch, 4.0);
        source.datagram = Some(response(&[(5, 9.0), (-38, 1.0)]));
        assert!(source.poll(&mut t).unwrap(), "negative indices down to -38 are valid");
        assert_eq!(t.pitch, 9.0);
    }

    #[test]
    fn a_failed_request_escapes_check_status() {
        // A send error while requesting the datarefs is not handled: it fails the
        // liveness test and ends the dispatcher's loop. Linux refuses UDP datagrams to port 0 (EINVAL).
        let mut source = XPlaneSource::new(0, XPlaneOptions::default());
        let error = source.is_running().unwrap_err();
        assert!(crate::udp_racing::unhandled::ends_dispatcher(&error), "{error}");
    }

    #[test]
    fn check_status_subscribes_and_reads_the_reply() {
        let xplane = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        xplane.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = xplane.local_addr().unwrap().port();
        let simulator = std::thread::spawn(move || {
            let mut buf = [0u8; REQUEST_LEN];
            let mut subscriber = None;
            for _ in 0..DATAREFS.len() {
                let (len, from) = xplane.recv_from(&mut buf).unwrap();
                assert_eq!(len, REQUEST_LEN);
                subscriber = Some(from);
            }
            xplane.send_to(&response(&[(5, 3.0)]), subscriber.unwrap()).unwrap();
        });
        let mut source = XPlaneSource::new(port, XPlaneOptions::default());
        assert!(source.is_running().unwrap());
        simulator.join().unwrap();
        let mut t = Telemetry::off();
        source.poll(&mut t).unwrap();
        assert_eq!(t.pitch, 3.0);
    }
}

//! Motion-platform drivers, the model table and the driver registry.
//!
//! The default motion path: the daemon calls
//! [`MotionRegistry::connect_pass`] once per [`CONNECT_INTERVAL`] and
//! [`MotionRegistry::send_stream`] once per motion frame, which goes to the model's driver.
//! Drivers implement [`MotionDriver`]; they read the platform model and write the
//! connection state through [`MotionContext`].
//!
//! The [`Supervisor`] (ramps, rate limit, timeouts) and the SMC3 position-feedback diagnostics
//! are not part of the default path; a caller has to opt
//! in explicitly.

pub mod actuon;
pub mod clock;
pub mod driver_support;
pub mod native;
pub mod odrive;
pub mod prosimu;
pub mod pt_actuator;
pub mod python;
pub mod scn6;
pub mod simtech;
pub mod smc3;
mod supervisor;
pub mod thanos;
pub mod tmbs;
pub mod vnm;
pub mod yaw_vr;

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use serde::Serialize;
use tracing::error;

use crate::discovery::{ClaimedPorts, ListPortInfo, PortInfo};
use crate::serial::{Link, LinkError, OpenOptions, SerialLink};
use clock::{Clock, SystemClock};

pub use supervisor::{Supervisor, SupervisorConfig, SupervisorState};

/// Errors from motion drivers.
#[derive(Debug, thiserror::Error)]
pub enum MotionError {
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error("no controller answered on {0}")]
    NotFound(String),
    #[error("{0}")]
    Protocol(String),
    /// A lookup or conversion failure of a dynamic value, encoded as `ClassName: message`.
    #[error("{0}")]
    Native(String),
    /// A value does not fit its wire field (an overflow error).
    #[error("value {value} does not fit in {bytes} byte(s)")]
    OutOfRange { value: i64, bytes: usize },
    /// The motor list is shorter than the driver indexes (an index error).
    #[error("driver needs {needed} motor values, got {got}")]
    TooFewMotors { needed: usize, got: usize },
    #[error("unknown motion model {0:?}")]
    UnknownModel(String),
    /// No driver is registered for the model's controller (a key error).
    #[error("no driver for controller {0:?}")]
    NoDriver(&'static str),
    /// An error together with the frames it passed through,
    /// outermost first (see [`MotionError::in_frames`]). It reads and compares as the error it wraps.
    #[error("{error}")]
    Traced { error: Box<MotionError>, frames: Vec<PyFrame> },
    /// An error that occurred while `context` was being handled: the
    /// handled error's traceback is printed in front of this one's (see [`MotionError::while_handling`]). It reads
    /// and compares as `error`.
    #[error("{error}")]
    Handling { context: Box<MotionError>, error: Box<MotionError> },
}

/// One frame of a traceback. A frame has no source
/// text: it prints as `  File "<file>", line <line>, in <module>.<function>`, where the
/// module is the file's dotted path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyFrame {
    /// The file name of the frame.
    pub file: &'static str,
    /// The function name of the frame.
    pub function: &'static str,
    pub line: u32,
}

impl PyFrame {
    pub const fn new(file: &'static str, function: &'static str, line: u32) -> Self {
        PyFrame { file, function, line }
    }

    /// The frame's line of a traceback, with its newline. A frame of a driver file
    /// (`backend/…`) names its function with the file's dotted path; a frame of any other file
    /// prints the plain function name. No source line follows either.
    pub fn text(&self) -> String {
        if !self.file.starts_with("backend/") {
            return format!("  File \"{}\", line {}, in {}\n", self.file, self.line, self.function);
        }
        let module = self.file.trim_end_matches(".py").replace('/', ".");
        format!("  File \"{}\", line {}, in {module}.{}\n", self.file, self.line, self.function)
    }
}

/// The serial port operation a driver step performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialCall {
    /// A write.
    Write,
    /// A read.
    Read,
}

/// The file name in the frames of serial port errors: the port-not-open error of a read or a write,
/// and the write timeout.
pub const PYSERIAL_PORT_FILE: &str = "serial\\serialwin32.py";
/// The file name in the frame of a serial port's set-up, which opens the port.
pub const PYSERIAL_BASE_FILE: &str = "serial\\serialutil.py";

/// The file name in the frames of the registry's own steps.
pub const MOTION_HELPERS_FILE: &str = "backend/controllers/motion_helpers.py";
/// The frame of the registry's connect, which every error of a
/// driver's connect passes before it is handled.
const HELPERS_CONNECT: PyFrame = PyFrame::new(MOTION_HELPERS_FILE, "connect", 100);

impl MotionError {
    /// The message of the error, where it is known: a key error prints its
    /// quoted key (`'thanos'`), a serial port error its message. Other errors
    /// print their own text.
    pub fn python_str(&self) -> String {
        match self.root() {
            MotionError::NoDriver(key) => format!("'{key}'"),
            // The serial text: `Write timeout` for a write that timed out, the closed-port text, else the
            // system error.
            MotionError::Link(link) => python::link_error_str(link),
            MotionError::Native(detail) => {
                detail.split_once(": ").map_or_else(|| detail.clone(), |(_, message)| message.to_owned())
            }
            MotionError::TooFewMotors { .. } => "list index out of range".into(),
            other => other.to_string(),
        }
    }

    /// The error itself, without the frames recorded around it.
    pub fn root(&self) -> &MotionError {
        match self {
            MotionError::Traced { error, .. } | MotionError::Handling { error, .. } => error.root(),
            other => other,
        }
    }

    /// The frames recorded for this error, outermost first; empty when none were.
    pub fn frames(&self) -> &[PyFrame] {
        match self {
            MotionError::Traced { frames, .. } => frames,
            MotionError::Handling { error, .. } => error.frames(),
            _ => &[],
        }
    }

    /// Records that the error passed through `outer` on its way out: these frames go in front of the ones
    /// already recorded. The place an error starts records its own frame; each caller the error leaves adds its
    /// frame with the line of the call.
    pub fn in_frames(self, outer: impl IntoIterator<Item = PyFrame>) -> MotionError {
        let mut frames: Vec<PyFrame> = outer.into_iter().collect();
        match self {
            MotionError::Traced { error, frames: inner } => {
                frames.extend(inner);
                MotionError::Traced { error, frames }
            }
            MotionError::Handling { context, error } => {
                MotionError::Handling { context, error: Box::new(error.in_frames(frames)) }
            }
            error => MotionError::Traced { error: Box::new(error), frames },
        }
    }

    /// [`in_frames`](Self::in_frames) for one frame.
    pub fn in_frame(self, outer: PyFrame) -> MotionError {
        self.in_frames([outer])
    }

    /// A port error of the operation `call` in the driver step at `caller`: the closed-port
    /// and write-timeout errors carry a fixed serial frame, so their traceback is
    /// `caller` and that frame. Any other port error (a system error, whose text is
    /// the platform's) stays without frames.
    pub fn in_serial_call(self, call: SerialCall, caller: PyFrame) -> MotionError {
        let line = match (self.root(), call) {
            (MotionError::Link(LinkError::Io(io)), SerialCall::Write) if io.kind() == std::io::ErrorKind::TimedOut => {
                325
            }
            (MotionError::Link(LinkError::Io(io)), _) if io.kind() == std::io::ErrorKind::NotConnected => match call {
                SerialCall::Write => 306,
                SerialCall::Read => 269,
            },
            _ => return self,
        };
        let function = match call {
            SerialCall::Write => "write",
            SerialCall::Read => "read",
        };
        self.in_frames([caller, PyFrame::new(PYSERIAL_PORT_FILE, function, line)])
    }

    /// The frame of a caller the error leaves, added only when the frames below it were recorded: an
    /// error whose starting frame is not known stays without frames rather than gaining a partial list.
    pub fn through(self, outer: PyFrame) -> MotionError {
        if self.frames().is_empty() {
            self
        } else {
            self.in_frame(outer)
        }
    }

    /// This error as one that occurred while `context` was being handled:
    /// its traceback text is the traceback of `context` — the frames from its
    /// handler down to its start — then `During handling of the above exception, another exception occurred:`
    /// and this error's traceback.
    pub fn while_handling(self, context: MotionError) -> MotionError {
        MotionError::Handling { context: Box::new(context), error: Box::new(self) }
    }

    /// What is printed in front of this error's own traceback: nothing when no
    /// error was being handled, else the handled error's traceback and the chaining sentence. `None`
    /// when an error was being handled whose frames or class are not known.
    pub fn handled_traceback(&self) -> Option<String> {
        let MotionError::Handling { context, .. } = self else {
            return Some(String::new());
        };
        let exception = context.exception_line().filter(|_| !context.frames().is_empty())?;
        let frames: String = context.frames().iter().map(PyFrame::text).collect();
        Some(format!(
            "{}Traceback (most recent call last):\n{frames}{exception}\n\nDuring handling of the above exception, \
             another exception occurred:\n\n",
            context.handled_traceback()?
        ))
    }

    /// The last line of the traceback, `<class>: <message>`, where the error class is known: the
    /// class a dynamic-value failure carries, `KeyError` for a model or controller the tables do not have,
    /// `IndexError` for a motor list shorter than the driver indexes, and the serial classes for a port error
    /// (their message is the platform's). `None` for an error without a known class.
    pub fn exception_line(&self) -> Option<String> {
        match self.root() {
            MotionError::Native(detail) => Some(detail.clone()),
            MotionError::NoDriver(key) => Some(format!("KeyError: '{key}'")),
            MotionError::UnknownModel(model) => Some(format!("KeyError: {}", python::str_repr(model))),
            MotionError::TooFewMotors { .. } => Some("IndexError: list index out of range".into()),
            MotionError::Link(link) => Some(match link {
                LinkError::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => {
                    "serial.serialutil.SerialTimeoutException: Write timeout".to_string()
                }
                LinkError::Io(io) if io.kind() == std::io::ErrorKind::NotConnected => {
                    format!("serial.serialutil.PortNotOpenError: {io}")
                }
                other => format!("serial.serialutil.SerialException: {}", python::link_error_str(other)),
            }),
            _ => None,
        }
    }

    /// The traceback text of this error as its handler holds it (what
    /// an error record in the log appends to its message, without a final newline): the
    /// chained error it occurred while handling, then `Traceback (most recent call last):`, the frames
    /// from the handler's own function down to the start, and the error line. `None` when the class or the
    /// frames are not known.
    pub fn caught_traceback(&self) -> Option<String> {
        let exception = self.exception_line().filter(|_| !self.frames().is_empty())?;
        let handled = self.handled_traceback()?;
        let frames: String = self.frames().iter().map(PyFrame::text).collect();
        Some(format!("{handled}Traceback (most recent call last):\n{frames}{exception}"))
    }

    /// The traceback text the registry prints for a failed connect, where every line of it is
    /// known: the header, the frame of the registry's connect, the frames recorded inside
    /// the driver and the error line. The frame lines have no source text.
    ///
    /// The key error of a controller without a driver has no deeper frame. For an error inside a
    /// driver the text is given only when the driver recorded its frames ([`MotionError::Traced`]) and the
    /// error class is known; `None` otherwise, rather than a traceback with frames missing.
    pub fn python_traceback(&self) -> Option<String> {
        let complete = matches!(self, MotionError::NoDriver(_)) || !self.frames().is_empty();
        let exception = self.exception_line().filter(|_| complete)?;
        let handled = self.handled_traceback()?;
        let frames: String = self.frames().iter().map(PyFrame::text).collect();
        Some(format!("{handled}Traceback (most recent call last):\n{}{frames}{exception}\n", HELPERS_CONNECT.text()))
    }
}

/// The motion status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum LinkStatus {
    /// `'Connected'`.
    Connected,
    /// `'Not connected'`.
    #[default]
    NotConnected,
    /// `'Not Connected'` (capital C): what the write-error handling of the PT CANBus and VNM drivers
    /// stores. It is a value of its own, so a change between it
    /// and `'Not connected'` is a change of the motion status; every test compares with
    /// `'Connected'` only, so it is "not connected" everywhere.
    NotConnectedAfterWriteError,
}

/// Connection state shared by the registry and the active driver.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LinkState {
    /// The motion status.
    pub status: LinkStatus,
    /// The motion ports: controller slot → port name.
    pub ports: BTreeMap<usize, String>,
}

impl LinkState {
    pub fn is_connected(&self) -> bool {
        self.status == LinkStatus::Connected
    }
}

/// The motion status and motion ports: the one shared state the registry, the driver calls and the
/// drivers' own threads all read and write
/// (for example the Yaw VR feedback loop disconnecting). A write is seen by every reader at once.
#[derive(Clone, Default)]
pub struct SharedLink(Arc<Mutex<LinkState>>);

impl SharedLink {
    fn lock(&self) -> std::sync::MutexGuard<'_, LinkState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current motion status and motion ports.
    pub fn get(&self) -> LinkState {
        self.lock().clone()
    }

    /// Whether the motion status is `'Connected'`.
    pub fn is_connected(&self) -> bool {
        self.lock().is_connected()
    }

    /// Changes the state in place.
    pub fn update<R>(&self, change: impl FnOnce(&mut LinkState) -> R) -> R {
        change(&mut self.lock())
    }

    /// Sets the status `'Not connected'` and empties the motion ports.
    pub fn drop_link(&self) {
        self.update(|link| {
            link.status = LinkStatus::NotConnected;
            link.ports.clear();
        });
    }

    /// Stores what a driver call wrote: each of the two keys the call changed (from `before` to `after`) takes
    /// the call's value; a key the call left alone keeps what is there now (a driver thread may have written it
    /// meanwhile).
    pub(crate) fn store_call(&self, before: &LinkState, after: LinkState) {
        self.update(|link| {
            if after.status != before.status {
                link.status = after.status;
            }
            if after.ports != before.ports {
                link.ports = after.ports;
            }
        });
    }
}

impl std::fmt::Debug for SharedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.lock().fmt(f)
    }
}

/// Handles compare equal: they carry no value of their own, only access to the shared one.
impl PartialEq for SharedLink {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

/// What a controller reported about itself at connect (SMC3: its version,
/// its model id and its number of motors). `None` fields were not reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ControllerIdentity {
    pub version: Option<i64>,
    pub model_id: Option<i64>,
    pub reported_motors: Option<i64>,
}

/// Last commanded actuator positions on the 10-bit scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ActuatorPositions {
    pub front_left: i64,
    pub front_right: i64,
    pub rear_left: i64,
    pub rear_right: i64,
    /// `m0` … `m5`.
    pub motors: [i64; 6],
}

/// What drivers need from the system: time, serial ports and datagrams. Tests substitute fakes.
pub trait MotionHost {
    fn clock(&mut self) -> &mut dyn Clock;
    /// The ports the scans list.
    fn serial_ports(&mut self) -> Vec<PortInfo>;
    /// The listing can fail while it is built (for example after a
    /// USB sysfs field vanishes). A failed enumeration is not an empty scan.
    fn serial_ports_checked(&mut self) -> Result<Vec<PortInfo>, MotionError> {
        Ok(self.serial_ports())
    }
    /// The listing entry of a listed port (`description`, `hwid`, the USB fields). Hosts that do not read
    /// the system derive it from the listing.
    fn port_details(&mut self, port: &PortInfo) -> ListPortInfo {
        ListPortInfo::from_listing(port)
    }
    /// Open a port exclusively.
    fn open_serial(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError>;
    /// Open a port without the exclusive lock: a port another
    /// object still holds opens again. Hosts without that distinction open as [`Self::open_serial`] does.
    fn open_serial_shared(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
        self.open_serial(path, options)
    }
    /// Ports held by accessory devices.
    fn claimed_ports(&self) -> ClaimedPorts;
    /// The `[APP] allowed_com_ports` setting.
    fn allowed_com_ports(&self) -> String;
    /// Send from the host's shared UDP socket.
    fn send_datagram(&mut self, target: SocketAddr, payload: &[u8]) -> std::io::Result<()>;
    /// Send from the UDP socket named `socket`, for drivers that own several sockets (each one its own
    /// ephemeral source port). Hosts without separate sockets use the shared one.
    fn send_datagram_from(
        &mut self,
        socket: DatagramSocket,
        target: SocketAddr,
        payload: &[u8],
    ) -> std::io::Result<()> {
        let _ = socket;
        self.send_datagram(target, payload)
    }
    /// Send from socket `index` of the numbered set `set`, for drivers that create one socket per destination
    /// object (each its own ephemeral source port). Hosts without separate sockets use [`Self::send_datagram_from`].
    fn send_datagram_numbered(
        &mut self,
        set: DatagramSocket,
        index: usize,
        target: SocketAddr,
        payload: &[u8],
    ) -> std::io::Result<()> {
        let _ = index;
        self.send_datagram_from(set, target, payload)
    }
    /// Close every socket of the numbered set `set` (the objects that owned them were dropped).
    fn close_datagram_set(&mut self, set: DatagramSocket) {
        let _ = set;
    }
}

/// Name of one sending UDP socket of a [`MotionHost`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DatagramSocket(pub &'static str);

impl DatagramSocket {
    /// The socket behind [`MotionHost::send_datagram`].
    pub const SHARED: DatagramSocket = DatagramSocket("shared");
}

/// The real system.
#[derive(Default)]
pub struct SystemHost {
    clock: SystemClock,
    /// Bound on first use, one per [`DatagramSocket`] (index 0) or per member of a numbered set.
    sockets: BTreeMap<(DatagramSocket, usize), UdpSocket>,
    listed_details: BTreeMap<String, ListPortInfo>,
    /// Kept current by the daemon.
    pub claimed: ClaimedPorts,
    /// Kept current by the daemon.
    pub allowed_com_ports: String,
}

impl MotionHost for SystemHost {
    fn clock(&mut self) -> &mut dyn Clock {
        &mut self.clock
    }

    fn serial_ports(&mut self) -> Vec<PortInfo> {
        self.serial_ports_checked().unwrap_or_default()
    }

    fn serial_ports_checked(&mut self) -> Result<Vec<PortInfo>, MotionError> {
        let listed =
            crate::discovery::ports_with_details().map_err(|error| MotionError::Protocol(error.to_string()))?;
        let (ports, details): (Vec<_>, Vec<_>) = listed.into_iter().unzip();
        self.listed_details = details.into_iter().map(|details| (details.device.clone(), details)).collect();
        Ok(ports)
    }

    fn port_details(&mut self, port: &PortInfo) -> ListPortInfo {
        self.listed_details.get(&port.path).cloned().unwrap_or_else(|| crate::discovery::port_details(&port.path))
    }

    fn open_serial(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
        Ok(Box::new(SerialLink::open(path, options)?))
    }

    fn open_serial_shared(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
        Ok(Box::new(SerialLink::open_shared(path, options)?))
    }

    fn claimed_ports(&self) -> ClaimedPorts {
        self.claimed.clone()
    }

    fn allowed_com_ports(&self) -> String {
        self.allowed_com_ports.clone()
    }

    fn send_datagram(&mut self, target: SocketAddr, payload: &[u8]) -> std::io::Result<()> {
        self.send_datagram_from(DatagramSocket::SHARED, target, payload)
    }

    fn send_datagram_from(
        &mut self,
        socket: DatagramSocket,
        target: SocketAddr,
        payload: &[u8],
    ) -> std::io::Result<()> {
        self.send_datagram_numbered(socket, 0, target, payload)
    }

    fn send_datagram_numbered(
        &mut self,
        set: DatagramSocket,
        index: usize,
        target: SocketAddr,
        payload: &[u8],
    ) -> std::io::Result<()> {
        let socket = match self.sockets.entry((set, index)) {
            std::collections::btree_map::Entry::Occupied(bound) => bound.into_mut(),
            std::collections::btree_map::Entry::Vacant(slot) => slot.insert(UdpSocket::bind(("0.0.0.0", 0))?),
        };
        socket.send_to(payload, target).map(|_| ())
    }

    fn close_datagram_set(&mut self, set: DatagramSocket) {
        self.sockets.retain(|(name, _), _| *name != set);
    }
}

/// Everything a driver call may read or update.
pub struct MotionContext<'a> {
    /// The selected platform model (`MOTIONBOX.model`).
    pub model: &'a ModelSpec,
    pub link: &'a mut LinkState,
    pub host: &'a mut dyn MotionHost,
}

/// A motion controller driver: one per kind of controller.
///
/// `motors` is the per-frame list of motor positions on the model's scale from 0 to its largest motor value
/// (12 slots); each driver uses the slots its hardware has.
pub trait MotionDriver: Send {
    /// Find and open the controller(s); on success set `ctx.link.status` and `ctx.link.ports`.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError>;
    fn disconnect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError>;
    /// One motion frame. A failed write sets `ctx.link.status` to `NotConnected`, which makes the next
    /// connect pass reconnect.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError>;
    /// One motion-test frame (Hardware page "Motion Test").
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError>;
    /// One motion frame on the caller's own dynamic list: the
    /// driver reads each slot where it uses it, writes its edits back
    /// into the list (every holder of the object sees them at once, also when a later step fails),
    /// and computes with arbitrary-width integers up to the packing of the frame.
    ///
    /// Every driver of this crate implements it. The default is the bridge for implementors that
    /// only have the `&[i64]` path: it requires every slot to be an `i64` up front.
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        native_bridge(self, motors, ctx, false)
    }
    /// One motion-test frame on the caller's own dynamic list; see
    /// [`send_stream_native`](Self::send_stream_native).
    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        native_bridge(self, motors, ctx, true)
    }
    /// The debug output of a frame (called while the Telemetry page's
    /// debug status is on). Every driver except SMC3 only returns `true`.
    fn debug_native(&mut self, motors: &LegacyValue, host: &mut dyn MotionHost) -> Result<(), MotionError> {
        let _ = (motors, host);
        Ok(())
    }
    /// The caller's motor list as the last `send_stream`/`send_test` left it, when the driver changed it. The
    /// drivers edit the list they are given in place (reverse switches, inverted TL, idle positions), and
    /// the motion pipeline keeps the edited list as its stream and its previous frame. Taking it clears it.
    fn take_edited_frame(&mut self) -> Option<Vec<i64>> {
        None
    }
    /// Hands the driver the registry's motion status and motion ports once, when the
    /// registry creates it, for drivers whose own threads read and write them (see [`SharedLink`]).
    fn attach_link(&mut self, link: SharedLink) {
        let _ = link;
    }
    /// Last commanded positions, for drivers that publish them.
    fn actuators_position(&self) -> Option<ActuatorPositions> {
        None
    }
    /// The actuator positions as the 16-bit drivers store them: the four corners in
    /// millimetres, each slot rescaled from 0..65535 to 0..stroke as a float (motion compensation reads them).
    fn actuators_travel(&self) -> Option<driver_support::CornerTravel> {
        None
    }
    /// What the connected controller reported about itself (for example the SMC3 version,
    /// model id and number of motors), for drivers that publish it.
    fn identity(&self) -> Option<ControllerIdentity> {
        None
    }
    /// How many controller ports a complete connection of `model` holds right now: the model's
    /// controller count, which some drivers change at runtime (Thanos raises it to 2 for
    /// a Thanos4U(M) with more than four motors, PROSIMU lowers it while the Dyadic TL is disabled). `None` keeps
    /// the static [`ModelSpec::controllers`]. The value is a signed integer, so it can drop to 0 or below.
    fn expected_controllers(&self, model: &ModelSpec) -> Option<i64> {
        let _ = model;
        None
    }
    /// The Hardware page's axis-assignment view (the actuator records, the assignment status and
    /// the port of the Dyadic TL axis), for drivers that publish it.
    fn assignment(&self) -> Option<AxisAssignment> {
        None
    }
    fn describe(&self) -> String;
}

/// One actuator record as the Axis Assignment dialog lists it
/// (`Actuator <key> - Axis <axis> - <com_port> - S/N <sn>`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AssignableActuator {
    /// The dictionary key (the actuator's position in the scan).
    pub key: usize,
    pub axis: String,
    pub com_port: String,
    pub sn: String,
}

/// What the axis-assigning drivers (Dyadic, PROSIMU) publish for the Hardware page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AxisAssignment {
    pub actuators: Vec<AssignableActuator>,
    /// The axis assignment status.
    pub status: bool,
    /// The port of the Dyadic TL axis (`''` when none was found).
    pub tl_port: String,
}

/// Settings a driver reads on every call while the daemon may change them.
#[derive(Debug, Default)]
pub struct LiveSettings<T>(Arc<RwLock<T>>);

impl<T> Clone for LiveSettings<T> {
    fn clone(&self) -> Self {
        LiveSettings(Arc::clone(&self.0))
    }
}

impl<T: Clone> LiveSettings<T> {
    pub fn new(value: T) -> Self {
        LiveSettings(Arc::new(RwLock::new(value)))
    }

    pub fn get(&self) -> T {
        self.0.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Reads part of the value without cloning all of it.
    pub fn read<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn set(&self, value: T) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = value;
    }

    pub fn update(&self, change: impl FnOnce(&mut T)) {
        change(&mut self.0.write().unwrap_or_else(PoisonError::into_inner));
    }
}

/// The controller a model names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Controller {
    None,
    Smc3,
    Thanos,
    YawVr,
    Scn6,
    PtaMotion,
    PtCan,
    Vnm,
    SimTechRevolution,
    Prs200,
    Prs200Sv,
    Prs200Prs50,
    Prs200TlSurge,
}

impl Controller {
    /// The controller key in the model table.
    pub fn key(self) -> &'static str {
        match self {
            Controller::None => "none",
            Controller::Smc3 => "smc3",
            Controller::Thanos => "thanos",
            Controller::YawVr => "yawvr",
            Controller::Scn6 => "scn6",
            Controller::PtaMotion => "ptamotion",
            Controller::PtCan => "ptcan",
            Controller::Vnm => "vnm",
            Controller::SimTechRevolution => "simtechrevolution",
            Controller::Prs200 => "prs200",
            Controller::Prs200Sv => "prs200_sv",
            Controller::Prs200Prs50 => "prs200_prs50",
            Controller::Prs200TlSurge => "prs200_tl_surge",
        }
    }
}

/// The transport type of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Transport {
    Serial,
    Network,
}

/// Driver-relevant fields of one platform model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelSpec {
    pub name: &'static str,
    pub manufacturer: &'static str,
    pub controller: Controller,
    pub transport: Transport,
    /// The number of motors.
    pub motors: usize,
    /// How many controller ports a complete connection holds.
    pub controllers: usize,
    pub max_motor_abs: i64,
    /// The DOF Reality model code.
    pub id: i64,
    /// Whether the model is present only when the SimRacingTech models are enabled.
    pub requires_simracingtech: bool,
}

/// Driver view of the shared model definitions, including conditional entries.
pub static MOTION_MODELS: std::sync::LazyLock<Vec<ModelSpec>> = std::sync::LazyLock::new(|| {
    use openrigcue_model::motion_models::all_motion_models;
    all_motion_models()
        .map(|(model, requires_simracingtech)| {
            let text = |key: &str| model.fields.get(key).and_then(|value| value.as_str()).expect("model field is text");
            let integer = |key: &str| {
                model.fields.get(key).and_then(|value| value.as_integer()).expect("model field is an integer")
            };
            let controller = [
                Controller::None,
                Controller::Smc3,
                Controller::Thanos,
                Controller::YawVr,
                Controller::Scn6,
                Controller::PtaMotion,
                Controller::PtCan,
                Controller::Vnm,
                Controller::SimTechRevolution,
                Controller::Prs200,
                Controller::Prs200Sv,
                Controller::Prs200Prs50,
                Controller::Prs200TlSurge,
            ]
            .into_iter()
            .find(|controller| controller.key() == text("controller"))
            .expect("model controller is registered");
            ModelSpec {
                name: &model.key,
                manufacturer: text("manufacturer"),
                controller,
                transport: match text("type") {
                    "serial" => Transport::Serial,
                    "network" => Transport::Network,
                    _ => panic!("model transport is registered"),
                },
                motors: usize::try_from(integer("nbr_motors")).expect("model motor count is nonnegative"),
                controllers: usize::try_from(integer("nbr_controllers"))
                    .expect("model controller count is nonnegative"),
                max_motor_abs: integer("max_motor_abs"),
                id: model.fields.get("id").and_then(|value| value.as_integer()).unwrap_or(0),
                requires_simracingtech,
            }
        })
        .collect()
});

/// The model called `name`, if the build defines it.
pub fn motion_model(name: &str, simracingtech_enabled: bool) -> Option<&'static ModelSpec> {
    MOTION_MODELS.iter().find(|m| m.name == name && (simracingtech_enabled || !m.requires_simracingtech))
}

/// The driver implementation a controller key resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum DriverKind {
    /// The dummy driver: accepts every call and does nothing.
    Dummy,
    Smc3,
    Thanos,
    /// The Actuon driver, which serves the `thanos` controller key for the Actuon company id.
    Actuon,
    YawVr,
    Dyadic,
    DyadicPrevious,
    PtaMotion,
    PtCan,
    Vnm,
    SimTechRevolution,
    Prs200Dyadic,
    Prs200DyadicSv,
    Prs200Prs50,
    Prs200DyadicTlSurge,
}

/// The facts that decide which drivers exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverEnvironment {
    /// The company id; `srs` by default.
    pub company_id: String,
    /// Whether the drivers other than `none` and `smc3` are registered.
    pub windows_drivers: bool,
    /// `[PROSIMU] use_scn6_legacy` at start-up.
    pub use_scn6_legacy: bool,
    /// Whether the SimRacingTech models are enabled.
    pub simracingtech_enabled: bool,
}

impl DriverEnvironment {
    /// The default environment on Linux: only `none` and `smc3` exist.
    pub fn srs_linux() -> Self {
        DriverEnvironment {
            company_id: "srs".into(),
            windows_drivers: false,
            use_scn6_legacy: false,
            simracingtech_enabled: false,
        }
    }

    /// Which implementation serves `controller`, or `None` when the environment has no entry for it.
    pub fn driver_for(&self, controller: Controller) -> Option<DriverKind> {
        let srs = self.company_id == "srs";
        let windows_srs = self.windows_drivers && srs;
        match controller {
            Controller::None => Some(DriverKind::Dummy),
            Controller::Smc3 => srs.then_some(DriverKind::Smc3),
            Controller::Thanos if self.windows_drivers => {
                Some(if self.company_id == "actuon" { DriverKind::Actuon } else { DriverKind::Thanos })
            }
            Controller::Thanos => None,
            _ if !windows_srs => None,
            Controller::Scn6 if self.use_scn6_legacy => Some(DriverKind::DyadicPrevious),
            Controller::Scn6 => Some(DriverKind::Dyadic),
            Controller::Vnm => Some(DriverKind::Vnm),
            Controller::PtaMotion => Some(DriverKind::PtaMotion),
            Controller::PtCan => Some(DriverKind::PtCan),
            Controller::YawVr => Some(DriverKind::YawVr),
            Controller::SimTechRevolution => Some(DriverKind::SimTechRevolution),
            Controller::Prs200 => Some(DriverKind::Prs200Dyadic),
            Controller::Prs200Sv => Some(DriverKind::Prs200DyadicSv),
            Controller::Prs200Prs50 => Some(DriverKind::Prs200Prs50),
            Controller::Prs200TlSurge => Some(DriverKind::Prs200DyadicTlSurge),
        }
    }
}

/// Builds a driver instance; called once per kind, the instance then lives as long as the registry
/// (a driver keeps its state for the whole process).
pub type DriverFactory = Box<dyn Fn() -> Box<dyn MotionDriver> + Send>;

/// Inputs of one connect pass (read from the state and the settings on every iteration).
#[derive(Debug, Clone, Copy)]
pub struct ConnectPassInputs<'a> {
    /// `[MOTIONBOX] model`.
    pub model: &'a str,
    /// Whether the motion switch is on.
    pub motion_switch_on: bool,
    /// `[MOTIONBOX] auto_search`.
    pub auto_search: bool,
    /// `[MOTIONBOX] enable_debug`.
    pub enable_debug: bool,
}

/// Pause between connect passes when no device-event monitor is used.
pub const CONNECT_INTERVAL: Duration = Duration::from_secs(1);

/// Wake-up timeout of the event-driven scan wait: `None` (wait for a serial device event) for serial models
/// and unknown models, 1 s for network models.
pub fn serial_event_wait_timeout(model: Option<&ModelSpec>) -> Option<Duration> {
    const NETWORK_MOTION_WAKE: Duration = Duration::from_secs(1);
    model.filter(|m| m.transport != Transport::Serial).map(|_| NETWORK_MOTION_WAKE)
}

/// Pause between frames of a hardware refresh loop for `[<hardware>] refresh_rate`, capped at 30 Hz off
/// Windows; `None` for a rate ≤ 0.
pub fn refresh_interval(refresh_rate: i64, windows: bool) -> Option<Duration> {
    const NON_WINDOWS_MAX_RATE: i64 = 30;
    let rate = if windows { refresh_rate } else { refresh_rate.min(NON_WINDOWS_MAX_RATE) };
    // The pause is `1.0 / rate` seconds, rounded up to whole nanoseconds.
    (rate > 0).then(|| openrigcue_model::python_time::sleep_duration(1.0 / rate as f64).ok()).flatten()
}

/// Selects the driver for the configured model and keeps the motion connection state.
pub struct MotionRegistry {
    environment: DriverEnvironment,
    factories: BTreeMap<DriverKind, DriverFactory>,
    drivers: BTreeMap<DriverKind, Box<dyn MotionDriver>>,
    link: SharedLink,
}

impl MotionRegistry {
    /// A registry with the `dummy` driver registered.
    pub fn new(environment: DriverEnvironment) -> Self {
        let mut registry = MotionRegistry {
            environment,
            factories: BTreeMap::new(),
            drivers: BTreeMap::new(),
            link: SharedLink::default(),
        };
        registry.register(DriverKind::Dummy, Box::new(|| Box::new(DummyDriver)));
        registry
    }

    /// Make `kind` available. A later registration replaces an earlier one that was not used yet.
    pub fn register(&mut self, kind: DriverKind, factory: DriverFactory) {
        self.factories.insert(kind, factory);
    }

    /// Connection state now (the motion status and the motion ports).
    pub fn link(&self) -> LinkState {
        self.link.get()
    }

    /// The connection state itself, which the drivers' own threads also write; a reader that holds it sees every
    /// change at once.
    pub fn shared_link(&self) -> SharedLink {
        self.link.clone()
    }

    /// What the Axis Assignment confirmation writes (the status `'Not connected'` and empty motion ports):
    /// the next connect pass connects again.
    pub fn mark_disconnected(&mut self) {
        self.link.drop_link();
    }

    pub fn model(&self, name: &str) -> Option<&'static ModelSpec> {
        motion_model(name, self.environment.simracingtech_enabled)
    }

    /// Both motion steps of one connect pass, back to back: [`connect_step`](Self::connect_step),
    /// then [`check_ports`](Self::check_ports). An error of the port check (the key error
    /// for a model the tables do not have) is only logged here; a
    /// caller that runs the whole connect loop calls the two steps itself, the port check after the accessory
    /// devices, and ends the loop on that error.
    pub fn connect_pass(&mut self, inputs: &ConnectPassInputs<'_>, host: &mut dyn MotionHost) {
        self.connect_step(inputs, host);
        if let Err(e) = self.check_ports(inputs, host) {
            error!("connect_controllers: {e}");
        }
    }

    /// The motion connect of the connect loop: a connect
    /// when not connected, the motion switch is on and auto-search is enabled. A connect error is printed:
    /// `error trying to connect from motion helpers` with the error's message,
    /// then its traceback (see [`MotionError::python_traceback`]).
    pub fn connect_step(&mut self, inputs: &ConnectPassInputs<'_>, host: &mut dyn MotionHost) {
        if !self.link.is_connected() && inputs.motion_switch_on && inputs.auto_search {
            if let Err(e) = self.connect(inputs.model, host) {
                python_print(&["error trying to connect from motion helpers", &e.python_str()]);
                if let Some(traceback) = e.python_traceback() {
                    python_print(&[&traceback]);
                }
            }
        }
    }

    /// The motion port check of the connect loop, comparing with the runtime
    /// controller count. While connected, the model is looked up first (a
    /// key error, here [`MotionError::UnknownModel`], for a model the tables do not have, before
    /// the debug switch is read); for a serial model outside debug mode, the controller ports still listed are
    /// counted: the expected number keeps the status `Connected`, any other clears the ports and drops it to
    /// `NotConnected`, so the next pass reconnects.
    pub fn check_ports(
        &mut self,
        inputs: &ConnectPassInputs<'_>,
        host: &mut dyn MotionHost,
    ) -> Result<(), MotionError> {
        if !self.link.is_connected() {
            return Ok(());
        }
        let spec = self.require_model(inputs.model)?;
        if spec.transport != Transport::Serial || inputs.enable_debug {
            return Ok(());
        }
        let held = self.link.get().ports;
        let present =
            host.serial_ports_checked()?.iter().filter(|port| held.values().any(|held| *held == port.path)).count();
        let expected = self.expected_controllers(spec);
        if i64::try_from(present).is_ok_and(|present| present == expected) {
            self.link.update(|link| link.status = LinkStatus::Connected);
        } else {
            self.link.update(|link| {
                link.ports.clear();
                link.status = LinkStatus::NotConnected;
            });
        }
        Ok(())
    }

    /// Clear the controller ports and let the model's driver connect. A model the build does not know falls
    /// back to the `None` model.
    pub fn connect(&mut self, model: &str, host: &mut dyn MotionHost) -> Result<(), MotionError> {
        self.link.update(|link| link.ports.clear());
        let spec = self.model(model).unwrap_or(&MOTION_MODELS[0]);
        self.with_driver(spec, host, |driver, ctx| driver.connect(ctx))
    }

    /// Lets the model's driver disconnect.
    pub fn disconnect(&mut self, model: &str, host: &mut dyn MotionHost) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| driver.disconnect(ctx))
    }

    /// Motion switch turned off: the driver disconnects, then the controller ports are forgotten. A driver error
    /// leaves the ports as they are.
    pub fn switch_off(&mut self, model: &str, host: &mut dyn MotionHost) -> Result<(), MotionError> {
        self.disconnect(model, host)?;
        self.link.update(|link| link.ports.clear());
        Ok(())
    }

    /// Hand one frame to the model's driver. Whether a frame is
    /// sent at all (game state, motion switch, motion status) is the caller's decision.
    pub fn send_stream(&mut self, model: &str, motors: &[i64], host: &mut dyn MotionHost) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| driver.send_stream(motors, ctx))
    }

    /// [`send_stream`](Self::send_stream) on the caller's list, which then holds what the driver left in it (the
    /// drivers edit the list in place; the motion pipeline keeps the result as its previous frame).
    pub fn send_stream_in_place(
        &mut self,
        model: &str,
        motors: &mut [i64],
        host: &mut dyn MotionHost,
    ) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| {
            let result = driver.send_stream(motors, ctx);
            apply_edits(driver, motors);
            result
        })
    }

    /// [`send_test`](Self::send_test) on the caller's list, which then holds what the driver left in it.
    pub fn send_test_in_place(
        &mut self,
        model: &str,
        motors: &mut [i64],
        host: &mut dyn MotionHost,
    ) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| {
            let result = driver.send_test(motors, ctx);
            apply_edits(driver, motors);
            result
        })
    }

    /// A stream frame of the model's driver on the
    /// caller's shared dynamic list: the driver reads and writes its slots in place,
    /// so the list holds every edit made before an error too.
    pub fn send_stream_native_in_place(
        &mut self,
        model: &str,
        motors: &mut LegacyValue,
        host: &mut dyn MotionHost,
    ) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| driver.send_stream_native(motors, ctx))
    }

    /// A test frame of the model's driver on the caller's
    /// shared dynamic list; see [`send_stream_native_in_place`](Self::send_stream_native_in_place).
    pub fn send_test_native_in_place(
        &mut self,
        model: &str,
        motors: &mut LegacyValue,
        host: &mut dyn MotionHost,
    ) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| driver.send_test_native(motors, ctx))
    }

    /// The debug output of the model's driver for a stream frame: a model the
    /// tables do not have is a key error, as is a controller without a driver;
    /// then the driver's debug output (the SMC3 UDP mirror; `true` for every other one).
    pub fn debug_native(
        &mut self,
        model: &str,
        motors: &LegacyValue,
        host: &mut dyn MotionHost,
    ) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| driver.debug_native(motors, ctx.host))
    }

    /// Hand one motion-test frame to the model's driver.
    pub fn send_test(&mut self, model: &str, motors: &[i64], host: &mut dyn MotionHost) -> Result<(), MotionError> {
        let spec = self.require_model(model)?;
        self.with_driver(spec, host, |driver, ctx| driver.send_test(motors, ctx))
    }

    /// Last commanded positions published by the model's driver, if it was used and publishes them.
    pub fn actuators_position(&self, model: &str) -> Option<ActuatorPositions> {
        let kind = self.environment.driver_for(self.model(model)?.controller)?;
        self.drivers.get(&kind)?.actuators_position()
    }

    /// Corner travel in millimetres published by the model's driver, if it was used and publishes it.
    pub fn actuators_travel(&self, model: &str) -> Option<driver_support::CornerTravel> {
        let kind = self.environment.driver_for(self.model(model)?.controller)?;
        self.drivers.get(&kind)?.actuators_travel()
    }

    /// Controller identity published by the model's driver, if it was used and publishes one.
    pub fn identity(&self, model: &str) -> Option<ControllerIdentity> {
        let kind = self.environment.driver_for(self.model(model)?.controller)?;
        self.drivers.get(&kind)?.identity()
    }

    /// Axis-assignment view published by the model's driver, if it was used and publishes one.
    pub fn assignment(&self, model: &str) -> Option<AxisAssignment> {
        let kind = self.environment.driver_for(self.model(model)?.controller)?;
        self.drivers.get(&kind)?.assignment()
    }

    fn require_model(&self, name: &str) -> Result<&'static ModelSpec, MotionError> {
        self.model(name).ok_or_else(|| MotionError::UnknownModel(name.to_string()))
    }

    /// Runs `call` on the model's driver (created on first use, when it also receives the shared link). The call
    /// works on the state as it is when the call starts; what it changes is stored at its end
    /// ([`SharedLink`]), so a driver thread's writes meanwhile are kept where the call did not overwrite them.
    fn with_driver<R>(
        &mut self,
        spec: &ModelSpec,
        host: &mut dyn MotionHost,
        call: impl FnOnce(&mut dyn MotionDriver, &mut MotionContext<'_>) -> Result<R, MotionError>,
    ) -> Result<R, MotionError> {
        let missing = || MotionError::NoDriver(spec.controller.key());
        let kind = self.environment.driver_for(spec.controller).ok_or_else(missing)?;
        if !self.drivers.contains_key(&kind) {
            let factory = self.factories.get(&kind).ok_or_else(missing)?;
            let mut driver = factory();
            driver.attach_link(self.link.clone());
            self.drivers.insert(kind, driver);
        }
        let driver = self.drivers.get_mut(&kind).expect("inserted above");
        let before = self.link.get();
        let mut link = before.clone();
        let mut ctx = MotionContext { model: spec, link: &mut link, host };
        let result = call(driver.as_mut(), &mut ctx);
        self.link.store_call(&before, link);
        result
    }

    /// The model's controller count at this moment: the model's driver's
    /// runtime count when it has one ([`MotionDriver::expected_controllers`]), else the table value.
    fn expected_controllers(&self, spec: &ModelSpec) -> i64 {
        self.environment
            .driver_for(spec.controller)
            .and_then(|kind| self.drivers.get(&kind))
            .and_then(|driver| driver.expected_controllers(spec))
            .unwrap_or(spec.controllers as i64)
    }
}

/// Prints the parts joined by spaces and a newline, on the process's standard output.
/// A failing stdout is ignored.
pub(crate) fn python_print(parts: &[&str]) {
    use std::io::Write;
    #[cfg(test)]
    if driver_support::testing::captured_print(&parts.join(" ")) {
        return;
    }
    let _ = writeln!(std::io::stdout().lock(), "{}", parts.join(" "));
}

/// Writes `text` as given (the logging error report) on
/// standard error. As with [`python_print`], the stream is the process's; a failing write is ignored.
pub(crate) fn python_stderr(text: &str) {
    use std::io::Write;
    #[cfg(test)]
    if driver_support::testing::captured_stderr(text) {
        return;
    }
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

/// The `&[i64]` path of a driver without a dynamic-value frame path, on a dynamic list: every slot must hold an
/// `i64` (an overflow error otherwise, before the driver runs); the driver's edits are written back into the list.
fn native_bridge<D: MotionDriver + ?Sized>(
    driver: &mut D,
    motors: &mut LegacyValue,
    ctx: &mut MotionContext<'_>,
    test: bool,
) -> Result<(), MotionError> {
    let length = motors.resolved().py_len().ok_or_else(|| {
        MotionError::Native(format!("TypeError: object of type '{}' has no len()", motors.qualified_type_name()))
    })?;
    let mut values = Vec::with_capacity(length);
    for index in 0..length {
        let value = native::item(motors, index)?;
        values.push(
            native::as_i64(&value).ok_or_else(|| {
                MotionError::Native("OverflowError: Python int too large to convert to C long".into())
            })?,
        );
    }
    let result = if test { driver.send_test(&values, ctx) } else { driver.send_stream(&values, ctx) };
    if let Some(edited) = driver.take_edited_frame() {
        for (index, (before, after)) in values.iter().zip(edited).enumerate() {
            if *before != after {
                native::assign(motors, index, LegacyValue::Int(after))?;
            }
        }
    }
    result
}

/// Copy the driver's in-place edits of the last frame into `motors`.
fn apply_edits(driver: &mut dyn MotionDriver, motors: &mut [i64]) {
    if let Some(edited) = driver.take_edited_frame() {
        for (slot, value) in motors.iter_mut().zip(edited) {
            *slot = value;
        }
    }
}

/// The `none` controller: accepts every call and does nothing.
pub struct DummyDriver;

impl MotionDriver for DummyDriver {
    fn connect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        Ok(())
    }
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        Ok(())
    }
    fn send_stream(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        Ok(())
    }
    fn send_test(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        Ok(())
    }
    fn describe(&self) -> String {
        "no motion controller".into()
    }
}

/// Fakes for driver tests: a scripted host, scripted devices behind ports, and a manual clock.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::{BTreeMap, VecDeque};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use super::clock::Clock;
    use super::{DatagramSocket, MotionError, MotionHost};
    use crate::discovery::{ClaimedPorts, ListPortInfo, PortInfo};
    use crate::serial::{Link, LinkError, OpenOptions};

    /// Clock that only advances when slept on; shared with the links for timestamps.
    #[derive(Clone, Default)]
    pub struct FakeClock(Arc<Mutex<Duration>>);

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.0.lock().unwrap()
        }
        fn sleep(&mut self, d: Duration) {
            *self.0.lock().unwrap() += d;
        }
    }

    /// Behaviour of the device behind a port.
    #[derive(Clone, Default)]
    pub struct Device {
        /// Reply to a write that equals the key exactly.
        pub replies: Vec<(Vec<u8>, Vec<u8>)>,
        /// Answer `[ver]` only from this send on (0 = the first one).
        pub ver_reply_from: usize,
        /// Bytes waiting right after every open.
        pub on_open: Vec<u8>,
        pub fail_open: bool,
        pub fail_writes: bool,
        /// Writes that fail when they equal one of these.
        pub fail_on: Vec<Vec<u8>>,
    }

    impl Device {
        pub fn replying(replies: &[(&[u8], &[u8])]) -> Self {
            Device { replies: replies.iter().map(|(c, r)| (c.to_vec(), r.to_vec())).collect(), ..Device::default() }
        }
    }

    /// Everything that happened on one port.
    #[derive(Default)]
    pub struct PortLog {
        pub opens: Vec<OpenOptions>,
        /// For each entry of `opens`: whether it was a shared (non-exclusive) open.
        pub shared: Vec<bool>,
        pub writes: Vec<(Duration, Vec<u8>)>,
        pub clears: usize,
    }

    impl PortLog {
        pub fn written(&self) -> Vec<Vec<u8>> {
            self.writes.iter().map(|(_, w)| w.clone()).collect()
        }
    }

    struct FakeLink {
        name: String,
        device: Device,
        log: Arc<Mutex<PortLog>>,
        clock: FakeClock,
        pending: VecDeque<u8>,
        ver_sends: usize,
    }

    impl Link for FakeLink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            if self.device.fail_writes || self.device.fail_on.iter().any(|f| f == bytes) {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "write timeout").into());
            }
            self.log.lock().unwrap().writes.push((self.clock.now(), bytes.to_vec()));
            if bytes == b"[ver]" {
                self.ver_sends += 1;
                if self.ver_sends <= self.device.ver_reply_from {
                    return Ok(());
                }
            }
            if let Some((_, reply)) = self.device.replies.iter().find(|(c, _)| c == bytes) {
                self.pending.extend(reply);
            }
            Ok(())
        }
        fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
            Ok(self.pending.drain(..).collect())
        }
        fn clear_input(&mut self) -> Result<(), LinkError> {
            self.log.lock().unwrap().clears += 1;
            self.pending.clear();
            Ok(())
        }
        fn name(&self) -> &str {
            &self.name
        }
    }

    #[derive(Default)]
    pub struct FakeHost {
        pub clock: FakeClock,
        pub ports: Vec<PortInfo>,
        pub listed_details: BTreeMap<String, ListPortInfo>,
        pub port_list_error: Option<String>,
        pub devices: BTreeMap<String, Device>,
        logs: BTreeMap<String, Arc<Mutex<PortLog>>>,
        pub datagrams: Vec<(SocketAddr, Vec<u8>)>,
        /// Sending socket of each entry of `datagrams`.
        pub datagram_sockets: Vec<DatagramSocket>,
        /// For each entry of `datagrams`: the member index and the generation of its set (counts the
        /// [`MotionHost::close_datagram_set`] calls), `(0, 0)` for a plain socket.
        pub datagram_members: Vec<(usize, usize)>,
        set_generations: BTreeMap<DatagramSocket, usize>,
        pub claimed: ClaimedPorts,
        pub allowed: String,
    }

    impl FakeHost {
        pub fn with_ports(paths: &[&str]) -> Self {
            FakeHost {
                ports: paths.iter().map(|p| PortInfo { path: p.to_string(), stable_path: None, usb: None }).collect(),
                ..FakeHost::default()
            }
        }

        pub fn device(mut self, path: &str, device: Device) -> Self {
            self.devices.insert(path.to_string(), device);
            self
        }

        pub fn log(&mut self, path: &str) -> MutexGuard<'_, PortLog> {
            self.logs.entry(path.to_string()).or_default().lock().unwrap()
        }

        pub fn now(&self) -> Duration {
            self.clock.now()
        }
    }

    impl MotionHost for FakeHost {
        fn clock(&mut self) -> &mut dyn Clock {
            &mut self.clock
        }
        fn serial_ports(&mut self) -> Vec<PortInfo> {
            self.ports.clone()
        }

        fn serial_ports_checked(&mut self) -> Result<Vec<PortInfo>, MotionError> {
            match &self.port_list_error {
                Some(message) => Err(MotionError::Protocol(message.clone())),
                None => Ok(self.ports.clone()),
            }
        }

        fn port_details(&mut self, port: &PortInfo) -> ListPortInfo {
            self.listed_details.get(&port.path).cloned().unwrap_or_else(|| ListPortInfo::from_listing(port))
        }
        fn open_serial(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
            self.open(path, options, false)
        }
        fn open_serial_shared(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
            self.open(path, options, true)
        }
        fn claimed_ports(&self) -> ClaimedPorts {
            self.claimed.clone()
        }
        fn allowed_com_ports(&self) -> String {
            self.allowed.clone()
        }
        fn send_datagram(&mut self, target: SocketAddr, payload: &[u8]) -> std::io::Result<()> {
            self.send_datagram_from(DatagramSocket::SHARED, target, payload)
        }
        fn send_datagram_from(
            &mut self,
            socket: DatagramSocket,
            target: SocketAddr,
            payload: &[u8],
        ) -> std::io::Result<()> {
            self.datagrams.push((target, payload.to_vec()));
            self.datagram_sockets.push(socket);
            self.datagram_members.push((0, 0));
            Ok(())
        }
        fn send_datagram_numbered(
            &mut self,
            set: DatagramSocket,
            index: usize,
            target: SocketAddr,
            payload: &[u8],
        ) -> std::io::Result<()> {
            self.send_datagram_from(set, target, payload)?;
            let generation = self.set_generations.get(&set).copied().unwrap_or_default();
            *self.datagram_members.last_mut().expect("pushed above") = (index, generation);
            Ok(())
        }
        fn close_datagram_set(&mut self, set: DatagramSocket) {
            *self.set_generations.entry(set).or_default() += 1;
        }
    }

    impl FakeHost {
        fn open(&mut self, path: &str, options: &OpenOptions, shared: bool) -> Result<Box<dyn Link>, LinkError> {
            let device = self.devices.get(path).cloned().unwrap_or_default();
            let log = Arc::clone(self.logs.entry(path.to_string()).or_default());
            log.lock().unwrap().opens.push(*options);
            log.lock().unwrap().shared.push(shared);
            if device.fail_open {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "busy").into());
            }
            Ok(Box::new(FakeLink {
                name: path.to_string(),
                pending: device.on_open.iter().copied().collect(),
                device,
                log,
                clock: self.clock.clone(),
                ver_sends: 0,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeHost;
    use super::*;
    use std::sync::Mutex;

    /// The model table: DOF Reality entries.
    #[test]
    fn dof_reality_models_match_the_original_table() {
        let p6 = motion_model("P6", false).unwrap();
        assert_eq!((p6.motors, p6.controllers, p6.id, p6.max_motor_abs), (6, 2, 6, 4095));
        assert_eq!(motion_model("P3U", false).unwrap().id, -1);
        assert_eq!(motion_model("H4", false).unwrap().motors, 4);
        assert!(motion_model("STR-SCN6150", false).is_none());
        assert_eq!(motion_model("STR-SCN6150", true).unwrap().controllers, 4);
        assert_eq!(motion_model("Yaw3", false).unwrap().transport, Transport::Network);
        assert_eq!(MOTION_MODELS.len(), 74);
    }

    /// A driver that edits the frame it is given in place.
    struct Editing;

    impl MotionDriver for Editing {
        fn connect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn send_stream(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Err(MotionError::Protocol("after the edit".into()))
        }
        fn send_test(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn take_edited_frame(&mut self) -> Option<Vec<i64>> {
            Some(vec![9, 8])
        }
        fn describe(&self) -> String {
            "editing".into()
        }
    }

    /// The registry keeps the list the driver edited (also when the driver failed after the edit).
    #[test]
    fn in_place_sends_return_the_drivers_edits() {
        let mut registry = MotionRegistry::new(DriverEnvironment::srs_linux());
        registry.register(DriverKind::Smc3, Box::new(|| Box::new(Editing)));
        let mut host = FakeHost::default();
        let mut motors = [1, 2, 3];
        assert!(registry.send_stream_in_place("P3", &mut motors, &mut host).is_err());
        assert_eq!(motors, [9, 8, 3]);
        let mut motors = [1, 2, 3];
        registry.send_test_in_place("P3", &mut motors, &mut host).unwrap();
        assert_eq!(motors, [9, 8, 3]);
    }

    /// Which driver serves each controller key in each environment.
    #[test]
    fn dispatch_table_follows_the_module_init_rules() {
        let linux = DriverEnvironment::srs_linux();
        assert_eq!(linux.driver_for(Controller::None), Some(DriverKind::Dummy));
        assert_eq!(linux.driver_for(Controller::Smc3), Some(DriverKind::Smc3));
        assert_eq!(linux.driver_for(Controller::Thanos), None);
        assert_eq!(linux.driver_for(Controller::Vnm), None);
        let windows = DriverEnvironment { windows_drivers: true, ..linux.clone() };
        assert_eq!(windows.driver_for(Controller::Thanos), Some(DriverKind::Thanos));
        assert_eq!(windows.driver_for(Controller::Scn6), Some(DriverKind::Dyadic));
        assert_eq!(windows.driver_for(Controller::Prs200TlSurge), Some(DriverKind::Prs200DyadicTlSurge));
        let legacy = DriverEnvironment { use_scn6_legacy: true, ..windows.clone() };
        assert_eq!(legacy.driver_for(Controller::Scn6), Some(DriverKind::DyadicPrevious));
        let actuon = DriverEnvironment { company_id: "actuon".into(), ..windows };
        assert_eq!(actuon.driver_for(Controller::Thanos), Some(DriverKind::Actuon));
        assert_eq!(actuon.driver_for(Controller::Smc3), None);
        assert_eq!(actuon.driver_for(Controller::YawVr), None);
    }

    /// The two SMC3 sockets: each named socket keeps its own source port, the same
    /// name reuses its socket. Loopback only.
    #[test]
    fn system_host_sends_from_one_socket_per_name() {
        let receiver = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let target = receiver.local_addr().unwrap();
        let mut host = SystemHost::default();
        let mut source_port = |socket| {
            host.send_datagram_from(socket, target, b"x").unwrap();
            let mut buf = [0u8; 4];
            receiver.recv_from(&mut buf).unwrap().1.port()
        };
        let first = source_port(DatagramSocket("first"));
        let second = source_port(DatagramSocket("second"));
        assert_ne!(first, second);
        assert_eq!(source_port(DatagramSocket("first")), first);
    }

    /// The refresh interval: 1/rate, capped at 30 Hz off Windows.
    #[test]
    fn refresh_interval_caps_rate_off_windows() {
        // The requested sleep rounds up to whole nanoseconds: 1/30 s is 33,333,334 ns, not 33,333,333.
        assert_eq!(refresh_interval(500, false), Some(Duration::from_nanos(33_333_334)));
        assert_eq!(refresh_interval(500, true), Some(Duration::from_nanos(2_000_000)));
        assert_eq!(refresh_interval(20, false), Some(Duration::from_nanos(50_000_000)));
        assert_eq!(refresh_interval(90, true), Some(Duration::from_nanos(11_111_112)));
        assert_eq!(refresh_interval(0, false), None);
    }

    /// The wake-up timeout of the event-driven scan wait.
    #[test]
    fn event_wait_timeout_only_for_network_models() {
        assert_eq!(serial_event_wait_timeout(motion_model("P3", false)), None);
        assert_eq!(serial_event_wait_timeout(motion_model("Yaw3", false)), Some(Duration::from_secs(1)));
        assert_eq!(serial_event_wait_timeout(None), None);
    }

    /// Records calls and connects to fixed ports, standing in for a real driver.
    struct ScriptedDriver {
        calls: Arc<Mutex<Vec<String>>>,
        ports: Vec<&'static str>,
    }

    impl MotionDriver for ScriptedDriver {
        fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.calls.lock().unwrap().push(format!("connect {:?}", ctx.link.ports));
            for (slot, port) in self.ports.iter().enumerate() {
                ctx.link.ports.insert(slot, port.to_string());
            }
            ctx.link.status = LinkStatus::Connected;
            Ok(())
        }
        fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.calls.lock().unwrap().push("disconnect".into());
            Ok(())
        }
        fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.calls.lock().unwrap().push(format!("stream {motors:?}"));
            ctx.link.status = LinkStatus::NotConnected;
            Ok(())
        }
        fn send_test(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn describe(&self) -> String {
            "scripted".into()
        }
    }

    fn registry(ports: Vec<&'static str>) -> (MotionRegistry, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = MotionRegistry::new(DriverEnvironment::srs_linux());
        let log = Arc::clone(&calls);
        registry.register(
            DriverKind::Smc3,
            Box::new(move || Box::new(ScriptedDriver { calls: Arc::clone(&log), ports: ports.clone() })),
        );
        (registry, calls)
    }

    fn inputs(model: &str) -> ConnectPassInputs<'_> {
        ConnectPassInputs { model, motion_switch_on: true, auto_search: true, enable_debug: false }
    }

    /// A connect pass: connect, then keep `Connected` while every port is present.
    #[test]
    fn connect_pass_connects_and_keeps_present_ports() {
        let (mut reg, calls) = registry(vec!["/dev/ttyUSB1"]);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]);
        reg.connect_pass(&inputs("P3"), &mut host);
        assert!(reg.link().is_connected());
        reg.connect_pass(&inputs("P3"), &mut host);
        assert_eq!(calls.lock().unwrap().len(), 1, "no reconnect while connected");
    }

    /// A connect pass: no connect when the switch is off or auto-search is disabled.
    #[test]
    fn connect_pass_respects_switch_and_auto_search() {
        let (mut reg, calls) = registry(vec!["/dev/ttyUSB1"]);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]);
        reg.connect_pass(&ConnectPassInputs { motion_switch_on: false, ..inputs("P3") }, &mut host);
        reg.connect_pass(&ConnectPassInputs { auto_search: false, ..inputs("P3") }, &mut host);
        assert!(calls.lock().unwrap().is_empty());
    }

    /// A connect pass: a two-box model with one port present is demoted and its ports cleared.
    #[test]
    fn connect_pass_demotes_incomplete_connections() {
        let (mut reg, _calls) = registry(vec!["/dev/ttyUSB1"]);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]);
        reg.connect_pass(&inputs("P6"), &mut host);
        assert_eq!(reg.link().status, LinkStatus::NotConnected);
        assert!(reg.link().ports.is_empty());
    }

    /// A driver with a thread of its own that can drop the link (like the Yaw VR feedback loop).
    struct ThreadedDriver {
        calls: Arc<Mutex<Vec<String>>>,
        link: Arc<Mutex<Option<SharedLink>>>,
    }

    impl MotionDriver for ThreadedDriver {
        fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.calls.lock().unwrap().push("connect".into());
            ctx.link.ports.insert(0, "10.0.0.2".into());
            ctx.link.status = LinkStatus::Connected;
            Ok(())
        }
        fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn send_stream(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.calls.lock().unwrap().push("stream".into());
            Ok(())
        }
        fn send_test(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            self.calls.lock().unwrap().push("test".into());
            Ok(())
        }
        fn attach_link(&mut self, link: SharedLink) {
            *self.link.lock().unwrap() = Some(link);
        }
        fn describe(&self) -> String {
            "threaded".into()
        }
    }

    /// A driver thread's `'Not connected'` status and empty motion ports are the registry's state at
    /// once (one shared state), and the next connect pass reconnects.
    #[test]
    fn a_driver_thread_drops_the_link_and_the_next_pass_reconnects() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let attached = Arc::new(Mutex::new(None));
        let mut registry = MotionRegistry::new(DriverEnvironment::srs_linux());
        let (log, handle) = (Arc::clone(&calls), Arc::clone(&attached));
        registry.register(
            DriverKind::Smc3,
            Box::new(move || Box::new(ThreadedDriver { calls: Arc::clone(&log), link: Arc::clone(&handle) })),
        );
        let mut host = FakeHost::default();
        let pass = ConnectPassInputs { enable_debug: true, ..inputs("P3") };
        let shared = registry.shared_link();
        registry.connect_pass(&pass, &mut host);
        let link = attached.lock().unwrap().clone().expect("attached when created");
        assert!(registry.link().is_connected());
        assert!(link.is_connected(), "the thread sees 'Connected'");
        assert!(shared.is_connected());

        link.drop_link();
        assert_eq!(registry.link().status, LinkStatus::NotConnected, "seen at once, without a registry call");
        assert!(shared.get().ports.is_empty());

        registry.connect_pass(&pass, &mut host);
        assert!(registry.link().is_connected());
        assert_eq!(calls.lock().unwrap().iter().filter(|c| *c == "connect").count(), 2);
        assert!(link.is_connected());

        // The UI's 'Not connected' is the thread's too.
        registry.mark_disconnected();
        assert!(!link.is_connected());
    }

    /// A key the driver call did not change keeps a thread's write made during the call.
    #[test]
    fn a_call_stores_only_the_keys_it_changed() {
        let link = SharedLink::default();
        link.update(|l| {
            l.status = LinkStatus::Connected;
            l.ports.insert(0, "a".into());
        });
        let before = link.get();
        let mut after = before.clone();
        after.ports.insert(1, "b".into());
        link.update(|l| l.status = LinkStatus::NotConnected);
        link.store_call(&before, after);
        assert_eq!(link.get().status, LinkStatus::NotConnected);
        assert_eq!(link.get().ports.len(), 2);
    }

    /// A driver that connects on fixed ports and reports a runtime controller count.
    struct CountingDriver {
        ports: Vec<&'static str>,
        expected: Option<i64>,
    }

    impl MotionDriver for CountingDriver {
        fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            for (slot, port) in self.ports.iter().enumerate() {
                ctx.link.ports.insert(slot, port.to_string());
            }
            ctx.link.status = LinkStatus::Connected;
            Ok(())
        }
        fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn send_stream(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn send_test(&mut self, _motors: &[i64], _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
            Ok(())
        }
        fn expected_controllers(&self, _model: &ModelSpec) -> Option<i64> {
            self.expected
        }
        fn describe(&self) -> String {
            "counting".into()
        }
    }

    fn counting_registry(ports: Vec<&'static str>, expected: Option<i64>) -> MotionRegistry {
        let mut registry = MotionRegistry::new(DriverEnvironment::srs_linux());
        registry
            .register(DriverKind::Smc3, Box::new(move || Box::new(CountingDriver { ports: ports.clone(), expected })));
        registry
    }

    /// A connect pass reads the model's controller count when it checks the ports,
    /// after the driver's connect may have changed it: a runtime count of 2 keeps a two-port connection of
    /// a one-controller model (the Thanos4U(M) increase), a count of 1 keeps a one-port connection of a two-controller
    /// model (PROSIMU lowering), and without a runtime count the table value decides as before.
    #[test]
    fn connect_pass_checks_ports_against_the_runtime_controller_count() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"]);
        let mut raised = counting_registry(vec!["/dev/ttyUSB1", "/dev/ttyUSB2"], Some(2));
        raised.connect_pass(&inputs("P3"), &mut host);
        assert!(raised.link().is_connected());
        assert_eq!(raised.link().ports.len(), 2);

        let mut lowered = counting_registry(vec!["/dev/ttyUSB1"], Some(1));
        lowered.connect_pass(&inputs("P6"), &mut host);
        assert!(lowered.link().is_connected());

        let mut table = counting_registry(vec!["/dev/ttyUSB1", "/dev/ttyUSB2"], None);
        table.connect_pass(&inputs("P3"), &mut host);
        assert_eq!(table.link().status, LinkStatus::NotConnected, "P3 has one controller in the table");

        // A count lowered to 0 (a signed integer, decremented on every call) never matches the held ports.
        let mut zero = counting_registry(vec!["/dev/ttyUSB1"], Some(0));
        zero.connect_pass(&inputs("P3"), &mut host);
        assert_eq!(zero.link().status, LinkStatus::NotConnected);
        assert!(zero.link().ports.is_empty());
    }

    /// A connect pass: debug mode skips the port check.
    #[test]
    fn connect_pass_skips_port_check_in_debug_mode() {
        let (mut reg, _calls) = registry(vec!["COMX", "COMY"]);
        let mut host = FakeHost::with_ports(&[]);
        reg.connect_pass(&ConnectPassInputs { enable_debug: true, ..inputs("P6") }, &mut host);
        assert!(reg.link().is_connected());
    }

    /// A connect pass: while connected, the model is looked up before
    /// the debug switch is read, so a model the tables do not have is a key error also in debug mode; while not
    /// connected nothing is looked up.
    #[test]
    fn check_ports_l257_unknown_model_raises_while_connected() {
        let (mut reg, _calls) = registry(vec!["COMX"]);
        let mut host = FakeHost::with_ports(&[]);
        let unknown = ConnectPassInputs { model: "no such model", enable_debug: true, ..inputs("P3") };
        assert!(reg.check_ports(&unknown, &mut host).is_ok(), "not connected: no lookup");
        reg.connect_step(&inputs("P3"), &mut host);
        assert!(reg.link().is_connected());
        assert!(matches!(reg.check_ports(&unknown, &mut host), Err(MotionError::UnknownModel(_))));
        assert!(reg.link().is_connected(), "the raise leaves the state as it is");
    }

    /// A connect clears the motion ports; a stream frame failure → reconnect on the next pass.
    #[test]
    fn write_failure_leads_to_reconnect_with_cleared_ports() {
        let (mut reg, calls) = registry(vec!["/dev/ttyUSB1"]);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]);
        reg.connect_pass(&inputs("P3"), &mut host);
        reg.send_stream("P3", &[1, 2, 3], &mut host).unwrap();
        assert!(!reg.link().is_connected());
        reg.connect_pass(&inputs("P3"), &mut host);
        let calls = calls.lock().unwrap();
        assert_eq!(calls[2], "connect {}");
        assert!(reg.link().is_connected());
    }

    /// Switch off → driver disconnect, then empty motion ports.
    #[test]
    fn switch_off_disconnects_and_clears_ports() {
        let (mut reg, calls) = registry(vec!["/dev/ttyUSB1"]);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]);
        reg.connect_pass(&inputs("P3"), &mut host);
        assert_eq!(reg.link().ports.len(), 1);
        reg.switch_off("P3", &mut host).unwrap();
        assert_eq!(calls.lock().unwrap().last().unwrap(), "disconnect");
        assert!(reg.link().ports.is_empty());
        assert!(matches!(reg.switch_off("nope", &mut host), Err(MotionError::UnknownModel(_))));
    }

    /// Connect: unknown model → `None` model → dummy driver; missing driver → error.
    #[test]
    fn connect_falls_back_to_none_model_and_reports_missing_drivers() {
        let mut reg = MotionRegistry::new(DriverEnvironment::srs_linux());
        let mut host = FakeHost::with_ports(&[]);
        reg.connect("no such model", &mut host).unwrap();
        assert!(!reg.link().is_connected());
        assert!(matches!(reg.connect("P3", &mut host), Err(MotionError::NoDriver("smc3"))));
        assert!(matches!(
            reg.connect("3 Actuators [2 Front + 1 Rear]", &mut host),
            Err(MotionError::NoDriver("thanos"))
        ));
        assert!(matches!(reg.send_stream("nope", &[], &mut host), Err(MotionError::UnknownModel(_))));
    }

    #[test]
    fn live_settings_are_shared_between_clones() {
        let a = LiveSettings::new(1);
        let b = a.clone();
        b.set(5);
        a.update(|v| *v += 1);
        assert_eq!(b.get(), 6);
    }

    fn native_list(values: Vec<LegacyValue>) -> LegacyValue {
        LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(values)))
    }

    /// A driver with only the `&[i64]` path reaches the dynamic-list APIs through the bridge: its edits
    /// are written into the caller's shared object (also when it failed after the edit); a slot no `i64` holds
    /// stops the bridge before the driver runs.
    #[test]
    fn native_in_place_sends_bridge_drivers_without_a_native_path() {
        let mut registry = MotionRegistry::new(DriverEnvironment::srs_linux());
        registry.register(DriverKind::Smc3, Box::new(|| Box::new(Editing)));
        let mut host = FakeHost::default();
        let mut list = native_list(vec![LegacyValue::Int(1), LegacyValue::Int(2), LegacyValue::Int(3)]);
        let alias = list.clone();
        assert!(registry.send_stream_native_in_place("P3", &mut list, &mut host).is_err());
        assert_eq!(alias.resolved(), LegacyValue::List(vec![9.into(), 8.into(), 3.into()]));
        registry.send_test_native_in_place("P3", &mut list, &mut host).unwrap();
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 70);
        let mut list = native_list(vec![wide.clone(), LegacyValue::Int(2)]);
        let error = registry.send_test_native_in_place("P3", &mut list, &mut host).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "OverflowError: Python int too large to convert to C long");
        assert_eq!(native::item(&list, 0).unwrap(), wide);
    }

    /// The debug output of the model's driver: the model lookup is a
    /// key error first, then a controller without a driver; every driver but SMC3 only
    /// returns `true`.
    #[test]
    fn native_debug_dispatch_follows_the_tables() {
        let mut registry = MotionRegistry::new(DriverEnvironment::srs_linux());
        let mut host = FakeHost::default();
        let list = native_list(vec![LegacyValue::Int(0)]);
        let unknown = registry.debug_native("not a model", &list, &mut host).unwrap_err();
        assert_eq!(unknown.exception_line().unwrap(), "KeyError: 'not a model'");
        let missing = registry.debug_native("P3", &list, &mut host).unwrap_err();
        assert_eq!(missing.exception_line().unwrap(), "KeyError: 'smc3'");
        registry.debug_native("None", &list, &mut host).unwrap();
        registry.register(DriverKind::Smc3, Box::new(|| Box::new(Editing)));
        registry.debug_native("P3", &list, &mut host).unwrap();
        assert!(host.datagrams.is_empty());
    }
}

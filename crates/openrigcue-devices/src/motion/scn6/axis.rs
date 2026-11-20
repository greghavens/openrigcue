//! Finding, powering and homing Dyadic SCN6 servo controllers over TMBS.
//!
//! The two Dyadic drivers and the traction-loss part of the three PRS200 Dyadic drivers
//! run the same scan over the actuator records: the steps and their order are the same,
//! the diagnostics are not. The standalone drivers log every step; the
//! PRS200 drivers print the power-on, setup and homing steps and log the rest. [`ScanModule`] selects the
//! driver whose text and routing a call uses.

use std::collections::BTreeMap;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use tracing::{debug, error};

use super::tmbs::{convert_sn, Command, ParseError, Register, TmbsLink, WRITE_TIMEOUT};
use crate::discovery::{ListPortInfo, PortInfo};
use crate::motion::clock::Clock;
use crate::motion::{python, python_print, MotionError, MotionHost, PyFrame};
use crate::serial::{ClosedLink, OpenOptions};

/// Axis number of every controller.
pub const AXIS: u8 = 0;
/// Model codes at or above this (`0x3037FE80`) are accepted ("correct model").
pub const MODEL_CODE_MIN: i64 = 0x3037_FE80;
/// Baud rates tried on each port, in order.
pub const BAUD_RATES: [u32; 2] = [9600, 115_200];
/// Iterations of the power-on poll (no pause between polls).
const POWER_ON_POLLS: usize = 50;
/// Iterations of the homing poll.
const HOMING_POLLS: usize = 50;
/// Pause between homing polls.
const HOMING_PAUSE: Duration = Duration::from_secs(1);
/// The USB identity the scans require in a port's `hwid` text.
const FT232R: &str = "0403:6001";

/// The driver whose connect is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanModule {
    /// The Dyadic driver.
    Dyadic,
    /// The previous-generation Dyadic driver.
    DyadicPrevious,
    /// The PRS200 Dyadic driver.
    Prs200,
    /// The PRS200 Dyadic SV driver.
    Prs200Sv,
    /// The PRS200 Dyadic driver with traction loss and surge.
    Prs200TlSurge,
}

/// The frame lines of the steps of a driver's connect whose errors leave it.
struct ConnectLines {
    /// The "trying to connect" message built from the port, its description and its hardware id (PRS200 drivers).
    hwid_concat: u32,
    /// The test for `0403:6001` in the hardware id.
    hwid_contains: u32,
    /// The ALRM flags read.
    alarm: u32,
    /// Movement cancellation, alarm reset, alarm check, servo off: four consecutive lines from here.
    alarm_reset: u32,
    /// Servo on.
    servo_on: u32,
    /// The serial number read.
    serial: u32,
    /// The homing pattern read (standalone drivers).
    homing_pattern: u32,
    /// The homing movement of an actuator's interface.
    move_org: u32,
    /// The opening of the protocol on a port.
    instance: u32,
    /// The disconnect after a failed open, which fails while no interface was ever
    /// opened.
    unbound_disconnect: u32,
    /// The model code read.
    model_code: u32,
    /// The "Main power ON" flag read of the power-on poll (standalone drivers).
    power_poll: u32,
    /// The "Homing completion status" flag read of the homing poll (the drivers
    /// that log it).
    homing_poll: u32,
}

impl ScanModule {
    /// The two standalone Dyadic drivers: every step is a logger call, and the homing pattern is read.
    fn standalone(self) -> bool {
        matches!(self, ScanModule::Dyadic | ScanModule::DyadicPrevious)
    }

    /// The file name in the frames of the driver's errors.
    pub fn file(self) -> &'static str {
        match self {
            ScanModule::Dyadic => "backend/controllers/dyadic.py",
            ScanModule::DyadicPrevious => "backend/controllers/dyadic_previous.py",
            ScanModule::Prs200 => "backend/controllers/prs200_dyadic.py",
            ScanModule::Prs200Sv => "backend/controllers/prs200_dyadic_sv.py",
            ScanModule::Prs200TlSurge => "backend/controllers/prs200_dyadic_tl_surge.py",
        }
    }

    /// The frame of the driver's connect at `line`.
    pub fn connect_frame(self, line: u32) -> PyFrame {
        PyFrame::new(self.file(), "connect", line)
    }

    /// The frame lines of each driver's connect.
    fn lines(self) -> ConnectLines {
        match self {
            ScanModule::Dyadic => ConnectLines {
                hwid_concat: 0,
                hwid_contains: 83,
                alarm: 137,
                alarm_reset: 139,
                servo_on: 145,
                serial: 170,
                homing_pattern: 173,
                move_org: 195,
                instance: 90,
                unbound_disconnect: 95,
                model_code: 105,
                power_poll: 149,
                homing_poll: 202,
            },
            ScanModule::DyadicPrevious => ConnectLines {
                hwid_concat: 0,
                hwid_contains: 88,
                alarm: 142,
                alarm_reset: 144,
                servo_on: 150,
                serial: 175,
                homing_pattern: 178,
                move_org: 200,
                instance: 95,
                unbound_disconnect: 100,
                model_code: 110,
                power_poll: 154,
                homing_poll: 207,
            },
            ScanModule::Prs200 | ScanModule::Prs200Sv => ConnectLines {
                hwid_concat: 140,
                hwid_contains: 143,
                alarm: 195,
                alarm_reset: 197,
                servo_on: 203,
                serial: 229,
                homing_pattern: 0,
                move_org: 241,
                instance: 147,
                unbound_disconnect: 152,
                model_code: 162,
                // The PRS200 drivers print the power-on and homing poll failures.
                power_poll: 0,
                homing_poll: 0,
            },
            ScanModule::Prs200TlSurge => ConnectLines {
                hwid_concat: 144,
                hwid_contains: 147,
                alarm: 199,
                alarm_reset: 201,
                servo_on: 207,
                serial: 233,
                homing_pattern: 0,
                move_org: 246,
                instance: 151,
                unbound_disconnect: 156,
                model_code: 166,
                power_poll: 0,
                homing_poll: 253,
            },
        }
    }

    /// A power-on or setup step: printed by the PRS200 drivers, logged at debug level by the
    /// standalone ones.
    fn step(self, message: &str) {
        if self.standalone() {
            debug!("{message}");
        } else {
            python_print(&[message]);
        }
    }
}

/// One actuator record. A connect creates it empty; the scan assigns its fields one
/// at a time, so a scan that fails leaves the fields assigned so far.
pub struct ActuatorRecord {
    /// The interface: `None` until a scan stores a protocol link. A stored link stays here after its
    /// port was closed.
    pub interface: Option<TmbsLink>,
    /// The port, `''` until the scan completes this record.
    pub com_port: String,
    /// The serial number text.
    pub sn: String,
    /// The axis number as text, once an interface is stored.
    pub axis: String,
    /// Speed and acceleration, `-1` until the first velocity write.
    pub speed: i64,
    pub accel: i64,
    /// The homing pattern, which only the standalone Dyadic drivers read.
    pub homing_pattern: Option<i64>,
}

impl Default for ActuatorRecord {
    /// The empty record: no interface, empty texts, speed and acceleration `-1`.
    fn default() -> Self {
        ActuatorRecord {
            interface: None,
            com_port: String::new(),
            sn: String::new(),
            axis: String::new(),
            speed: -1,
            accel: -1,
            homing_pattern: None,
        }
    }
}

impl std::fmt::Debug for ActuatorRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActuatorRecord")
            .field("interface", &self.interface.as_ref().map(TmbsLink::port))
            .field("com_port", &self.com_port)
            .field("sn", &self.sn)
            .field("axis", &self.axis)
            .field("homing_pattern", &self.homing_pattern)
            .finish()
    }
}

impl ActuatorRecord {
    /// The record's interface for the operation `method`: a record no scan filled holds none, which is an error.
    pub fn link(&mut self, method: &str) -> Result<&mut TmbsLink, MotionError> {
        self.interface.as_mut().ok_or_else(|| none_attribute(method))
    }

    /// Disconnects the stored link: its port is closed, the record keeps the link.
    fn close(&mut self) {
        if let Some(link) = &mut self.interface {
            let name = link.port().to_string();
            *link = TmbsLink::new(Box::new(ClosedLink::new(&name)));
        }
    }

    /// The record as the scans print or log it. They do so right after resetting the records, so `'new_speed'`
    /// and `'new_accel'`, which only the frame path assigns, are still `None`.
    fn python_repr(&self) -> String {
        let interface = match &self.interface {
            // The interface prints as an object with its address in sixteen upper-case hexadecimal
            // digits. The object here is this record's link.
            Some(link) => {
                format!("<backend.controllers.pytbmscom.TMBSCOMProtocol object at 0x{:016X}>", link.object_address())
            }
            None => "None".to_string(),
        };
        let mut text = format!(
            "{{'interface': {interface}, 'com_port': {}, 'sn': {}, 'axis': {}, 'speed': {}, 'accel': {}, \
             'new_speed': None, 'new_accel': None",
            python::str_repr(&self.com_port),
            python::str_repr(&self.sn),
            python::str_repr(&self.axis),
            self.speed,
            self.accel,
        );
        if let Some(pattern) = self.homing_pattern {
            text.push_str(&format!(", 'homing_pattern': {pattern}"));
        }
        text.push('}');
        text
    }
}

/// The attribute error of an operation on a record without an interface.
fn none_attribute(method: &str) -> MotionError {
    MotionError::Native(format!("AttributeError: 'NoneType' object has no attribute '{method}'"))
}

/// The error of a reply that is not hexadecimal where nothing handles it: a value error, with
/// the protocol frames it passed. A value that does not fit an `i64`
/// keeps its own text.
pub(crate) fn uncaught(error: ParseError) -> MotionError {
    if error.is_value_error() {
        MotionError::Native(format!("ValueError: {error}")).in_frames(error.frames().iter().copied())
    } else {
        MotionError::Protocol(error.to_string())
    }
}

/// The actuator records as text: `{0: {…}, 1: {…}}`.
pub fn actuators_repr(records: &[ActuatorRecord]) -> String {
    let items: Vec<String> =
        records.iter().enumerate().map(|(key, record)| format!("{key}: {}", record.python_repr())).collect();
    format!("{{{}}}", items.join(", "))
}

/// The motion ports as text: integer keys, quoted port names.
pub fn ports_repr(ports: &BTreeMap<usize, String>) -> String {
    let items: Vec<String> = ports.iter().map(|(key, port)| format!("{key}: {}", python::str_repr(port))).collect();
    format!("{{{}}}", items.join(", "))
}

/// What the current interface of a connect refers to.
enum Interface {
    /// None opened yet: using it is an error.
    Unbound,
    /// An object no record holds; every path that leaves it here has already disconnected it.
    Detached,
    /// The object the record of this key holds.
    Record(usize),
}

/// The scan loop of the connects: assign an SCN6 to each record whose `com_port` is empty.
///
/// The records are visited in order (outer loop); each one tries
/// every listed port at both baud rates, skipping a port already in use and a port whose `hwid` lacks
/// `0403:6001`. The search does not stop at a match: each later identification replaces the record's
/// interface (the object it replaces is dropped, which closes its port), so a record ends with the last
/// matching port, and the port it gave up is free again for the following records.
///
/// A port that cannot be opened disconnects the current interface, which is still
/// the one of the previous attempt (an error, handled and logged, when there is none yet). If
/// that interface is the one a record holds, the record keeps it with its port closed.
///
/// Only the errors of the model-code read and test are handled. An error of a later step (the ALRM flags read,
/// the alarm commands, servo on, the serial number and homing pattern reads: a reply that is not hexadecimal)
/// ends the connect as the returned error; the record keeps the fields assigned before it.
pub fn scan_records(
    host: &mut dyn MotionHost,
    records: &mut [ActuatorRecord],
    module: ScanModule,
) -> Result<(), MotionError> {
    if module.standalone() {
        debug!("list serial ports");
    }
    // The listed ports, each with its name, description and hardware id.
    let ports: Vec<(PortInfo, ListPortInfo)> = host
        .serial_ports_checked()?
        .into_iter()
        .map(|port| {
            let details = host.port_details(&port);
            (port, details)
        })
        .collect();
    if module == ScanModule::Prs200TlSurge {
        // A copy of the actuator records is printed.
        python_print(&["temp_dict", &actuators_repr(records)]);
    }
    let mut used: BTreeMap<usize, String> = BTreeMap::new();
    let mut interface = Interface::Unbound;
    for key in 0..records.len() {
        if module.standalone() {
            debug!("Trying to find com_port of controller {key}");
        }
        if !records[key].com_port.is_empty() {
            continue;
        }
        if module.standalone() {
            debug!("Com port is empty, will look for serial ports");
        }
        for (port, details) in &ports {
            for baud in BAUD_RATES {
                let path = port.path.as_str();
                if module.standalone() {
                    let held: Vec<String> = used.values().map(|port| python::str_repr(port)).collect();
                    debug!(
                        "Trying to instance {path} baud_rate: {baud} {} {} dict_values([{}])",
                        details.description,
                        details.hwid_text(),
                        held.join(", ")
                    );
                } else {
                    // The "trying to connect" message with the port, its description and its hardware id.
                    let hwid = details.hwid.as_deref().ok_or_else(|| {
                        MotionError::Native("TypeError: can only concatenate str (not \"NoneType\") to str".into())
                            .in_frame(module.connect_frame(module.lines().hwid_concat))
                    })?;
                    debug!("trying to connect to {path} {} {hwid}", details.description);
                }
                if used.values().any(|held| held == path) {
                    continue;
                }
                if module.standalone() {
                    debug!("{path} is not being used by another controller");
                }
                let ftdi = details.hwid_contains(FT232R).map_err(|message| {
                    MotionError::Native(format!("TypeError: {message}"))
                        .in_frame(module.connect_frame(module.lines().hwid_contains))
                })?;
                if !ftdi {
                    continue;
                }
                if module.standalone() {
                    debug!("The address is valid");
                }
                debug!("Trying to instance {path} baud_rate: {baud}");
                let options = OpenOptions { baud_rate: baud, write_timeout: Some(WRITE_TIMEOUT) };
                let mut link = match host.open_serial(path, &options) {
                    Ok(link) => TmbsLink::new(link),
                    Err(e) => {
                        open_failed(module, e, &interface, records);
                        continue;
                    }
                };
                interface = Interface::Detached;
                if module.standalone() {
                    debug!("Time to check the protocol");
                }
                if !correct_model(&mut link, host.clock(), module) {
                    // The interface is disconnected: dropping the link closes the port.
                    continue;
                }
                power_on(&mut link, host.clock(), module)?;
                module.step("set iterface");
                // The object the record held until now is dropped here (its port closes).
                records[key].interface = Some(link);
                interface = Interface::Record(key);
                module.step("set port no");
                used.insert(key, path.to_string());
                module.step("set default axis");
                records[key].axis = AXIS.to_string();
                module.step("set sn");
                let lines = module.lines();
                let serial =
                    records[key].link("readSerialNumber")?.read_register(AXIS, Register::SerialNumber, host.clock());
                let serial = serial.map_err(|e| uncaught(e).in_frame(module.connect_frame(lines.serial)))?;
                records[key].sn = convert_sn(serial);
                if module.standalone() {
                    debug!("read homing pattern");
                    let pattern = records[key].link("readHomingPattern")?.read_register(
                        AXIS,
                        Register::HomingPattern,
                        host.clock(),
                    );
                    let pattern =
                        pattern.map_err(|e| uncaught(e).in_frame(module.connect_frame(lines.homing_pattern)))?;
                    records[key].homing_pattern = Some(pattern);
                    debug!("homing pattern: {pattern}");
                }
                module.step("set comport");
                records[key].com_port = path.to_string();
                if module.standalone() {
                    // The records and the "done with" line are logged at debug level.
                    debug!("{}", actuators_repr(records));
                    debug!("done with {path}");
                } else {
                    python_print(&[&actuators_repr(records)]);
                    python_print(&["done with", path]);
                }
            }
        }
    }
    Ok(())
}

/// Handles a port that did not open: log it, then disconnect the current interface, whose own
/// error is handled too.
fn open_failed(
    module: ScanModule,
    failure: crate::serial::LinkError,
    interface: &Interface,
    records: &mut [ActuatorRecord],
) {
    use crate::motion::driver_support::{exception_record, unbound_interface};
    let detail = python::link_error_str(&failure);
    let lines = module.lines();
    let opened = crate::motion::tmbs::open_exception(failure, module.connect_frame(lines.instance));
    match module {
        // The error is logged with its text as an argument. The
        // message has no placeholder for its argument, so no record is written; the failed
        // formatting is reported on standard error, chained to the open failure being handled.
        ScanModule::Dyadic => crate::motion::driver_support::connect_logging_argument_error(
            "Error while instancing the serial port, will disconnect",
            &[detail],
            &opened,
        ),
        // The error is logged with its text appended to the message: the
        // record carries the open failure's traceback.
        ScanModule::DyadicPrevious => error!(
            "{}",
            exception_record(
                &format!("Error while instancing the serial port, will disconnect {detail}"),
                &opened,
                None
            )
        ),
        // The error is logged as an invalid serial port, with its text.
        ScanModule::Prs200 | ScanModule::Prs200Sv | ScanModule::Prs200TlSurge => {
            error!("{}", exception_record(&format!("invalid serial port {detail}"), &opened, None));
        }
    }
    // The disconnect runs while the open failure is being handled, so its error
    // is chained to that failure.
    let unbound = || unbound_interface(module.connect_frame(lines.unbound_disconnect));
    match interface {
        Interface::Record(key) => records[*key].close(),
        Interface::Detached => {}
        Interface::Unbound if module == ScanModule::DyadicPrevious => {
            // The disconnect error is logged with its text, that of an interface never opened.
            error!(
                "{}",
                exception_record(
                    "error while disconnecting local variable 'interface' referenced before assignment",
                    &unbound(),
                    Some(&opened)
                )
            );
        }
        // The disconnect error is logged without its text.
        Interface::Unbound => error!("{}", exception_record("error while disconnecting", &unbound(), Some(&opened))),
    }
}

/// The model code check: the read, its log line, the zero test and the threshold test; errors are handled here.
/// `false` when the port is not an SCN6; the caller then disconnects the interface and moves on.
fn correct_model(link: &mut TmbsLink, clock: &mut dyn Clock, module: ScanModule) -> bool {
    let model_code = match link.read_register(AXIS, Register::ModelCode, clock) {
        Ok(model_code) => model_code,
        Err(e) => {
            // The error is logged ("error while reading model code not dyadic") with the
            // traceback of the read.
            let raised = uncaught(e).in_frame(module.connect_frame(module.lines().model_code));
            error!(
                "{}",
                crate::motion::driver_support::exception_record(
                    "error while reading model code not dyadic",
                    &raised,
                    None
                )
            );
            return false;
        }
    };
    if module.standalone() {
        debug!("model_code: {model_code}");
    } else {
        debug!("model_code {model_code}");
    }
    if model_code == 0 {
        debug!("error while reading model code not dyadic");
        return false;
    }
    if model_code < MODEL_CODE_MIN {
        return false;
    }
    debug!("correct model");
    true
}

/// Alarm reset, servo on and the power-on poll (the steps between the model code check and the record
/// assignments). A reply that is not hexadecimal is an error here.
fn power_on(link: &mut TmbsLink, clock: &mut dyn Clock, module: ScanModule) -> Result<(), MotionError> {
    let lines = module.lines();
    let raised_at = |line: u32| move |e: ParseError| uncaught(e).in_frame(module.connect_frame(line));
    if link.read_register(AXIS, Register::Alarm, clock).map_err(raised_at(lines.alarm))? != 0 {
        debug!("there is an alarm, trying to reset");
        let commands =
            [Command::CancelMovement, Command::StatusCheck, Command::InternalStatusReset, Command::Servo(false)];
        for (line, command) in (lines.alarm_reset..).zip(commands) {
            link.action(AXIS, &command, clock).map_err(raised_at(line))?;
        }
    }
    debug!("{}", if module.standalone() { "sending set_son" } else { "set_son" });
    link.action(AXIS, &Command::Servo(true), clock).map_err(raised_at(lines.servo_on))?;
    // Fifty polls of the "Main power ON" flag without a pause; a poll's error is handled.
    for _ in 0..POWER_ON_POLLS {
        let poll = module.connect_frame(lines.power_poll);
        match status_flag(Some(&mut *link), clock, |flags| flags.main_power_on, poll) {
            Ok(true) => {
                module.step("done");
                break;
            }
            Ok(false) => module.step("turning on"),
            // The error is logged ("error while powering on") with its text.
            Err(raised) if module.standalone() => error!(
                "{}",
                crate::motion::driver_support::exception_record(
                    &format!("error while powering on {}", raised.python_str()),
                    &raised,
                    None
                )
            ),
            Err(raised) => python_print(&["error while powering on", &raised.python_str()]),
        }
    }
    module.step("seton is done");
    Ok(())
}

/// The STAT flag `<flag>` read at `frame`, or the error of that read:
/// a missing interface (an attribute error), an unanswered read (a type error)
/// or a reply that is not hexadecimal (a value error with the protocol's frames).
fn status_flag(
    link: Option<&mut TmbsLink>,
    clock: &mut dyn Clock,
    flag: impl FnOnce(super::tmbs::StatFlags) -> bool,
    frame: PyFrame,
) -> Result<bool, MotionError> {
    let raised = |text: &str| MotionError::Native(text.into()).in_frame(frame);
    let link = link.ok_or_else(|| raised("AttributeError: 'NoneType' object has no attribute 'readSTATflags'"))?;
    match link.read_status(AXIS, clock) {
        Ok(Some(flags)) => Ok(flag(flags)),
        Ok(None) => Err(raised("TypeError: 'int' object is not subscriptable")),
        Err(e) => Err(uncaught(e).in_frame(frame)),
    }
}

/// The homing movement of an actuator. The error of its reply is not handled: a reply that
/// is not hexadecimal ends the connect, as does a record without an interface.
pub fn move_org(
    record: &mut ActuatorRecord,
    mode: &str,
    clock: &mut dyn Clock,
    module: ScanModule,
) -> Result<(), MotionError> {
    let frame = module.connect_frame(module.lines().move_org);
    record
        .link("move_org")
        .map_err(|e| e.in_frame(frame))?
        .action(AXIS, &Command::Homing(mode.to_string()), clock)
        .map(|_| ())
        .map_err(|e| uncaught(e).in_frame(frame))
}

/// The homing poll of one actuator: the "Homing completion status" flag up to 50 times, 1 s
/// apart; running out of polls is not an error.
///
/// - PRS200 and PRS200 SV: `homing` is printed while it runs, `done` when complete, which stops the poll;
///   an error prints `error while homing` with its text and stops.
/// - PRS200 with traction loss and surge: the same steps as log lines (`homing <key>`,
///   `homing completed<key>`, and `error while homing <text>` as an error record).
/// - The standalone Dyadic drivers: the same log lines, but an error waits 1 s and polls again.
///
/// The line before the poll (`homing`, printed or logged with the key) belongs to it as well.
pub fn wait_for_homing(record: &mut ActuatorRecord, key: usize, clock: &mut dyn Clock, module: ScanModule) {
    let prints = matches!(module, ScanModule::Prs200 | ScanModule::Prs200Sv);
    let running = || {
        if prints {
            python_print(&["homing"]);
        } else {
            debug!("homing {key}");
        }
    };
    running();
    for _ in 0..HOMING_POLLS {
        let poll = module.connect_frame(module.lines().homing_poll);
        match status_flag(record.interface.as_mut(), clock, |flags| flags.homing_complete, poll) {
            Ok(true) => {
                if prints {
                    python_print(&["done"]);
                } else {
                    debug!("homing completed{key}");
                }
                return;
            }
            Ok(false) => {
                running();
                clock.sleep(HOMING_PAUSE);
            }
            Err(raised) => {
                let detail = raised.python_str();
                if prints {
                    python_print(&["error while homing", &detail]);
                } else {
                    // The error is logged ("error while homing") with its text.
                    error!(
                        "{}",
                        crate::motion::driver_support::exception_record(
                            &format!("error while homing {detail}"),
                            &raised,
                            None
                        )
                    );
                }
                if module.standalone() {
                    clock.sleep(HOMING_PAUSE);
                } else {
                    return;
                }
            }
        }
    }
}

/// `n` as text of at least two digits, the homing mode the Dyadic driver sends.
pub fn zfill2(n: i64) -> String {
    // Only a single non-negative digit is shorter than two characters.
    format!("{n:02}")
}

/// The key of the axis-assignment entry for this axis and serial number: the axis assignment maps
/// an assignment-option index (as text) to `"<axis>,<serial>"`.
///
/// An entry without a comma (the dialog's `'None'` for an option without an
/// actuator) ends the lookup with `-1` when it is reached before a match. The entries are visited in key order, the
/// order in which the dialog writes its options.
pub fn assigned_key<'a>(
    assignment: &'a std::collections::BTreeMap<String, String>,
    axis: &str,
    serial: &str,
) -> Option<&'a str> {
    for (key, value) in assignment {
        let mut parts = value.split(',');
        let (a, s) = (parts.next()?, parts.next()?);
        if a == axis && s == serial {
            return Some(key.as_str());
        }
    }
    None
}

/// The integer key of the assignment entry for this axis, `-1` without one.
pub fn assigned_id(
    assignment: &std::collections::BTreeMap<String, String>,
    axis: &str,
    serial: &str,
) -> Result<LegacyValue, MotionError> {
    match assigned_key(assignment, axis, serial) {
        // The key as an integer, at any width.
        Some(key) => crate::motion::native::to_int(&LegacyValue::Str(key.to_owned())),
        None => Ok(LegacyValue::Int(-1)),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A scripted SCN6 controller behind a FakeHost port.

    use super::super::tmbs::testing::{reply, request};
    use super::super::tmbs::{Command, Register};
    use crate::motion::testing::Device;

    /// Replies of a powered, homed SCN6 with serial number `sn` and homing pattern `pattern`.
    pub fn scn6_device(sn: u32, pattern: u32) -> Device {
        let read = |r: Register| request(&Command::Read(r));
        let replies: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (read(Register::ModelCode), reply('0', "R4808976450")),
            (read(Register::Alarm), reply('0', "OK00000000")),
            (read(Register::Status), reply('0', "R40000000B")),
            (read(Register::SerialNumber), reply('0', &format!("R4{sn:08X}"))),
            (read(Register::HomingPattern), reply('0', &format!("R4{pattern:08X}"))),
            (request(&Command::Servo(true)), reply('0', "Q10B000000")),
            (request(&Command::Homing(format!("{pattern:02}"))), reply('0', "O10B000000")),
            (request(&Command::Homing("07".into())), reply('0', "O10B000000")),
        ];
        Device { replies, ..Device::default() }
    }

    /// Adds a velocity reply for `speed`/`accel` to a device.
    pub fn with_velocity(mut device: Device, speed: i64, accel: i64) -> Device {
        device.replies.push((request(&Command::Velocity { speed, accel }), reply('0', "V10B000000")));
        device
    }
}

#[cfg(test)]
mod tests {
    use super::testing::scn6_device;
    use super::*;
    use crate::motion::driver_support::testing::{capture_debug_messages, capture_diagnostics, ftdi_port, Diagnostic};
    use crate::motion::testing::{Device, FakeClock, FakeHost};
    use std::collections::BTreeMap;

    fn host(devices: &[(&str, Device)]) -> FakeHost {
        let mut host = FakeHost::default();
        for (path, device) in devices {
            host.ports.push(ftdi_port(path));
            host = host.device(path, device.clone());
        }
        host
    }

    fn records(count: usize) -> Vec<ActuatorRecord> {
        (0..count).map(|_| ActuatorRecord::default()).collect()
    }

    fn scan(host: &mut FakeHost, slots: usize, module: ScanModule) -> Vec<ActuatorRecord> {
        let mut records = records(slots);
        scan_records(host, &mut records, module).unwrap();
        records
    }

    #[test]
    fn scan_reads_model_code_powers_on_and_fills_the_record() {
        // Dyadic: a model code >= 0x3037FE80 (808976000) → servo on, poll 'Main power ON', then the
        // record fields in order, the homing pattern included.
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x00ABCDEF, 7))]);
        let found = scan(&mut h, 1, ScanModule::Dyadic);
        assert_eq!(found[0].sn, "00ABCDEF");
        assert_eq!(found[0].axis, "0");
        assert_eq!(found[0].com_port, "/dev/ttyUSB3");
        assert_eq!(found[0].homing_pattern, Some(7));
        assert_eq!(h.log("/dev/ttyUSB3").opens[0].baud_rate, 9600);
        // The PRS200 drivers do not read the homing pattern.
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x00ABCDEF, 7))]);
        let found = scan(&mut h, 1, ScanModule::Prs200);
        assert_eq!(found[0].homing_pattern, None);
        let pattern = super::super::tmbs::testing::request(&Command::Read(Register::HomingPattern));
        assert!(!h.log("/dev/ttyUSB3").written().contains(&pattern));
    }

    #[test]
    fn standalone_scan_logs_every_step_in_source_order() {
        // Dyadic, one record and one answering port: every step is a logger call, the
        // second baud rate only reaches the first line (the port is already in use).
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x11, 7))]);
        let mut found = records(1);
        let diagnostics = capture_diagnostics(|| scan_records(&mut h, &mut found, ScanModule::Dyadic).unwrap());
        let address = found[0].interface.as_ref().unwrap().object_address();
        let actuators = format!(
            "{{0: {{'interface': <backend.controllers.pytbmscom.TMBSCOMProtocol object at 0x{address:016X}>, \
             'com_port': '/dev/ttyUSB3', 'sn': '00000011', 'axis': '0', 'speed': -1, 'accel': -1, 'new_speed': None, \
             'new_accel': None, 'homing_pattern': 7}}}}"
        );
        let expected = [
            Diagnostic::log("list serial ports"),
            Diagnostic::log("Trying to find com_port of controller 0"),
            Diagnostic::log("Com port is empty, will look for serial ports"),
            Diagnostic::log(
                "Trying to instance /dev/ttyUSB3 baud_rate: 9600 ttyUSB3 USB VID:PID=0403:6001 dict_values([])",
            ),
            Diagnostic::log("/dev/ttyUSB3 is not being used by another controller"),
            Diagnostic::log("The address is valid"),
            Diagnostic::log("Trying to instance /dev/ttyUSB3 baud_rate: 9600"),
            Diagnostic::log("Time to check the protocol"),
            Diagnostic::log("model_code: 34503877712"),
            Diagnostic::log("correct model"),
            Diagnostic::print("Normal operation"),
            Diagnostic::log("sending set_son"),
            Diagnostic::log("done"),
            Diagnostic::log("seton is done"),
            Diagnostic::log("set iterface"),
            Diagnostic::log("set port no"),
            Diagnostic::log("set default axis"),
            Diagnostic::log("set sn"),
            Diagnostic::log("read homing pattern"),
            Diagnostic::log("homing pattern: 7"),
            Diagnostic::log("set comport"),
            Diagnostic::log(&actuators),
            Diagnostic::log("done with /dev/ttyUSB3"),
            Diagnostic::log(
                "Trying to instance /dev/ttyUSB3 baud_rate: 115200 ttyUSB3 USB VID:PID=0403:6001 \
                 dict_values(['/dev/ttyUSB3'])",
            ),
        ];
        assert_eq!(diagnostics, expected);
    }

    #[test]
    fn standalone_power_poll_logs_busy_and_error_polls_without_pausing() {
        // Dyadic: 'turning on' while the flag is clear, an error record with the error's
        // text for an unanswered status read; fifty polls, then 'seton is done' either way.
        let status = super::super::tmbs::testing::request(&Command::Read(Register::Status));
        for (answer, line) in [
            (Some(super::super::tmbs::testing::reply('0', "R400000002")), "turning on"),
            (
                None,
                "error while powering on 'int' object is not subscriptable\nTraceback (most recent call last):\n  \
                 File \"backend/controllers/dyadic.py\", line 149, in backend.controllers.dyadic.connect\n\
                 TypeError: 'int' object is not subscriptable",
            ),
        ] {
            let mut device = scn6_device(1, 7);
            device.replies.retain(|(command, _)| *command != status);
            if let Some(answer) = answer {
                device.replies.push((status.clone(), answer));
            }
            let mut h = host(&[("/dev/ttyUSB3", device)]);
            let mut found = records(1);
            let messages = capture_debug_messages(|| scan_records(&mut h, &mut found, ScanModule::Dyadic).unwrap());
            let polls: Vec<&str> = messages
                .iter()
                .map(String::as_str)
                .skip_while(|message| *message != "sending set_son")
                .skip(1)
                .take_while(|message| *message != "seton is done")
                .collect();
            assert_eq!(polls, vec![line; 50]);
            assert_eq!(found[0].com_port, "/dev/ttyUSB3", "the record is completed after the poll");
        }
    }

    #[test]
    fn successful_power_poll_reports_completion_after_acknowledgement() {
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(1, 7))]);
        let messages = capture_debug_messages(|| {
            scan(&mut h, 1, ScanModule::Dyadic);
        });
        let acknowledgement = messages.iter().position(|message| message == "done").unwrap();
        let completion = messages.iter().position(|message| message == "seton is done").unwrap();
        assert!(acknowledgement < completion);
        assert_eq!(messages.iter().filter(|message| *message == "seton is done").count(), 1);
    }

    #[test]
    fn scan_rejects_silent_ports_and_low_model_codes() {
        // The model code is 0 without a reply ('not dyadic'); a code below the threshold is dropped.
        let mut h = host(&[("/dev/ttyUSB3", Device::default())]);
        assert!(scan(&mut h, 1, ScanModule::Dyadic)[0].interface.is_none());
        let mut low = scn6_device(1, 7);
        low.replies[0].1 = super::super::tmbs::testing::reply('0', "R4000000010");
        let mut h = host(&[("/dev/ttyUSB3", low)]);
        let found = scan(&mut h, 1, ScanModule::Dyadic);
        assert!(found[0].interface.is_none());
        assert_eq!(found[0].com_port, "");
    }

    #[test]
    fn model_code_errors_stay_inside_the_scan() {
        // The model code check: a reply that is not hexadecimal is logged as an error and the port is
        // dropped at both baud rates; nothing leaves the scan.
        let mut device = scn6_device(1, 7);
        device.replies[0].1 = super::super::tmbs::testing::reply('0', "R4ZZZZZZZZ");
        let mut h = host(&[("/dev/ttyUSB3", device)]);
        let mut found = records(1);
        let messages = capture_debug_messages(|| scan_records(&mut h, &mut found, ScanModule::Prs200).unwrap());
        let record = format!(
            "error while reading model code not dyadic\n{}",
            connect_traceback(
                "prs200_dyadic",
                162,
                "  File \"backend/controllers/pytbmscom.py\", line 561, in \
                 backend.controllers.pytbmscom.TMBSCOMProtocol.readModelCode\n",
                "ValueError: invalid literal for int() with base 16: b'ZZZZZZZZ'"
            )
        );
        assert_eq!(messages.iter().filter(|message| **message == record).count(), 2, "{messages:?}");
        assert!(!messages.iter().any(|message| message.starts_with("model_code")));
        assert!(found[0].interface.is_none());
    }

    #[test]
    fn later_errors_leave_the_scan_with_the_fields_assigned_so_far() {
        // Not handled: the ALRM flags read, servo on, the serial number read and (Dyadic) the homing pattern
        // read. Each value error ends the scan; the record holds what the preceding steps assigned.
        let read = |register| super::super::tmbs::testing::request(&Command::Read(register));
        let servo_on = super::super::tmbs::testing::request(&Command::Servo(true));
        let cases = [
            (read(Register::Alarm), false, "", None),
            (servo_on, false, "", None),
            (read(Register::SerialNumber), true, "", None),
            (read(Register::HomingPattern), true, "00000011", None),
        ];
        for (command, stored, sn, pattern) in cases {
            let mut device = scn6_device(0x11, 7);
            for (request, answer) in &mut device.replies {
                if *request == command {
                    *answer = super::super::tmbs::testing::reply('0', "R4ZZZZZZZZ");
                }
            }
            let mut h = host(&[("/dev/ttyUSB3", device)]);
            let mut found = records(1);
            let error = scan_records(&mut h, &mut found, ScanModule::Dyadic).unwrap_err();
            assert!(
                matches!(error.root(), MotionError::Native(detail) if detail.starts_with("ValueError: invalid literal for int() with base 16: b'")),
                "{error:?}"
            );
            assert_eq!(found[0].interface.is_some(), stored);
            assert_eq!(found[0].axis, if stored { "0" } else { "" });
            assert_eq!(found[0].sn, sn);
            assert_eq!(found[0].homing_pattern, pattern);
            assert_eq!(found[0].com_port, "", "com_port is the last field the scan assigns");
            assert_eq!(h.log("/dev/ttyUSB3").opens.len(), 1, "the second baud rate is not tried");
        }
    }

    #[test]
    fn a_failing_second_match_leaves_a_mixed_record() {
        // Record 0 completes on A; B then replaces the interface before its serial number read fails:
        // the record holds B's interface with A's serial number and port.
        let mut second = scn6_device(0x22, 7);
        second.replies[3].1 = super::super::tmbs::testing::reply('0', "R4ZZZZZZZZ");
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x11, 7)), ("/dev/ttyUSB4", second)]);
        let mut found = records(1);
        assert!(scan_records(&mut h, &mut found, ScanModule::Prs200).is_err());
        assert_eq!(found[0].interface.as_ref().unwrap().port(), "/dev/ttyUSB4");
        assert_eq!((found[0].com_port.as_str(), found[0].sn.as_str()), ("/dev/ttyUSB3", "00000011"));
    }

    #[test]
    fn model_code_threshold_boundary() {
        // Dyadic and PRS200: a model code >= 0x3037FE80 (808976000) is "correct model".
        let with_code = |code: i64| {
            let mut device = scn6_device(1, 7);
            device.replies[0].1 = super::super::tmbs::testing::reply('0', &format!("R4{code:08X}"));
            let mut h = host(&[("/dev/ttyUSB3", device)]);
            scan(&mut h, 1, ScanModule::Dyadic)[0].interface.is_some()
        };
        assert_eq!(MODEL_CODE_MIN, 808_976_000);
        assert!(!with_code(808_975_999));
        assert!(with_code(808_976_000));
        assert!(!with_code(808_500_000));
    }

    fn ports_of(slots: &[ActuatorRecord]) -> Vec<&str> {
        slots.iter().map(|record| record.com_port.as_str()).collect()
    }

    #[test]
    fn scan_two_slots_two_controllers_swaps_the_ports() {
        // Dyadic: the records are the outer loop and the ports
        // the inner one, which does not stop after 'done with'. Slot 0 takes A, then B replaces it
        // (which frees A); slot 1 re-opens A. A port already in use is skipped, so the second
        // baud rate of a matched port is not tried.
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x11, 7)), ("/dev/ttyUSB4", scn6_device(0x22, 7))]);
        let slots = scan(&mut h, 2, ScanModule::Dyadic);
        assert_eq!(ports_of(&slots), ["/dev/ttyUSB4", "/dev/ttyUSB3"]);
        assert_eq!(slots[1].sn, "00000011");
        let bauds = |h: &mut FakeHost, p: &str| -> Vec<u32> { h.log(p).opens.iter().map(|o| o.baud_rate).collect() };
        assert_eq!(bauds(&mut h, "/dev/ttyUSB3"), [9600, 9600]);
        assert_eq!(bauds(&mut h, "/dev/ttyUSB4"), [9600]);
    }

    #[test]
    fn scan_two_slots_three_controllers_last_free_match_wins() {
        // Dyadic with ports [A, B, C]: slot 0 ends on C (A, then B, then C); slot 1 takes A, then
        // B; C is held by slot 0.
        let mut h = host(&[
            ("/dev/ttyUSB3", scn6_device(0x11, 7)),
            ("/dev/ttyUSB4", scn6_device(0x22, 7)),
            ("/dev/ttyUSB5", scn6_device(0x33, 7)),
        ]);
        let slots = scan(&mut h, 2, ScanModule::Dyadic);
        assert_eq!(ports_of(&slots), ["/dev/ttyUSB5", "/dev/ttyUSB4"]);
        assert_eq!(h.log("/dev/ttyUSB3").opens.len(), 2);
        assert_eq!(h.log("/dev/ttyUSB4").opens.len(), 2);
        assert_eq!(h.log("/dev/ttyUSB5").opens.len(), 1);
    }

    #[test]
    fn scan_with_one_slot_keeps_the_last_controller() {
        // PRS200 (traction-loss axis; the port loop does not stop after 'done with'):
        // A is opened and powered, then B replaces it.
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x11, 7)), ("/dev/ttyUSB4", scn6_device(0x22, 7))]);
        let slots = scan(&mut h, 1, ScanModule::Prs200);
        assert_eq!(ports_of(&slots), ["/dev/ttyUSB4"]);
        assert_eq!(h.log("/dev/ttyUSB3").opens.len(), 1);
        assert_eq!(h.log("/dev/ttyUSB4").opens.len(), 1);
    }

    #[test]
    fn scan_keeps_a_match_when_later_ports_fail() {
        // A silent port after the match leaves the slot on the earlier controller.
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x11, 7)), ("/dev/ttyUSB4", Device::default())]);
        let slots = scan(&mut h, 1, ScanModule::Dyadic);
        assert_eq!(ports_of(&slots), ["/dev/ttyUSB3"]);
        assert_eq!(h.log("/dev/ttyUSB4").opens.len(), 2);
    }

    #[test]
    fn open_failure_closes_the_interface_of_the_previous_attempt() {
        // The open-failure handling disconnects the current interface, here the axis slot 0 just took: the slot
        // keeps it with a closed port.
        let busy = Device { fail_open: true, ..Device::default() };
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(0x11, 7)), ("/dev/ttyUSB4", busy.clone())]);
        let mut slots = scan(&mut h, 1, ScanModule::Dyadic);
        assert_eq!(ports_of(&slots), ["/dev/ttyUSB3"]);
        let link = slots[0].interface.as_mut().unwrap();
        // The protocol handles the failed write: the STAT read is unanswered.
        assert_eq!(link.read_status(AXIS, &mut FakeClock::default()).unwrap(), None);
        // A rejected attempt in between makes that (already closed) link the current interface: the slot stays open.
        let mut h = host(&[
            ("/dev/ttyUSB3", scn6_device(0x11, 7)),
            ("/dev/ttyUSB4", Device::default()),
            ("/dev/ttyUSB5", busy),
        ]);
        let mut slots = scan(&mut h, 1, ScanModule::Dyadic);
        let link = slots[0].interface.as_mut().unwrap();
        assert!(link.read_status(AXIS, &mut FakeClock::default()).unwrap().is_some());
    }

    /// The traceback of the failed open of `module` at `line`, as its handling holds it.
    fn open_failure_traceback(module: &str, line: u32) -> String {
        format!(
            "Traceback (most recent call last):\n  File \"backend/controllers/{module}.py\", line {line}, in \
             backend.controllers.{module}.connect\n  File \"backend/controllers/pytbmscom.py\", line 18, in \
             backend.controllers.pytbmscom.TMBSCOMProtocol.__init__\n  File \"serial\\serialwin32.py\", line 33, \
             in __init__\n  File \"serial\\serialutil.py\", line 244, in __init__\n  File \
             \"serial\\serialwin32.py\", line 64, in open\nserial.serialutil.SerialException: busy"
        )
    }

    /// The traceback of `exception`, from the connect of `module` at `line` (and handled there), with the
    /// protocol frames `inner` below it.
    fn connect_traceback(module: &str, line: u32, inner: &str, exception: &str) -> String {
        format!(
            "Traceback (most recent call last):\n  File \"backend/controllers/{module}.py\", line {line}, in \
             backend.controllers.{module}.connect\n{inner}{exception}"
        )
    }

    const CHAINED: &str = "\n\nDuring handling of the above exception, another exception occurred:\n\n";
    const UNBOUND: &str = "UnboundLocalError: local variable 'interface' referenced before assignment";

    #[test]
    fn open_failure_messages_differ_per_module() {
        // The first attempt of a scan fails to open: there is no interface yet, so the
        // disconnect fails too and is handled on its own.
        // Each record carries its error: the open failure, and for the disconnect its own error
        // chained to it (the disconnect runs while the open failure is being handled).
        let busy = Device { fail_open: true, ..Device::default() };
        let records_of = |file: &str, instance: u32, unbound: u32, open_message: Option<&str>, disconnect: &str| {
            let opened = open_failure_traceback(file, instance);
            let mut expected: Vec<String> = open_message.map(|m| format!("{m}\n{opened}")).into_iter().collect();
            expected.push(format!("{disconnect}\n{opened}{CHAINED}{}", connect_traceback(file, unbound, "", UNBOUND)));
            expected
        };
        for (module, expected) in [
            (
                ScanModule::Prs200,
                records_of("prs200_dyadic", 147, 152, Some("invalid serial port busy"), "error while disconnecting"),
            ),
            (
                ScanModule::Prs200Sv,
                records_of("prs200_dyadic_sv", 147, 152, Some("invalid serial port busy"), "error while disconnecting"),
            ),
            (
                ScanModule::Prs200TlSurge,
                records_of(
                    "prs200_dyadic_tl_surge",
                    151,
                    156,
                    Some("invalid serial port busy"),
                    "error while disconnecting",
                ),
            ),
            // The Dyadic driver's error log writes no record (see the next test).
            (ScanModule::Dyadic, records_of("dyadic", 90, 95, None, "error while disconnecting")),
            (
                ScanModule::DyadicPrevious,
                records_of(
                    "dyadic_previous",
                    95,
                    100,
                    Some("Error while instancing the serial port, will disconnect busy"),
                    "error while disconnecting local variable 'interface' referenced before assignment",
                ),
            ),
        ] {
            let mut h = host(&[("/dev/ttyUSB3", busy.clone())]);
            let mut found = records(1);
            let messages = capture_debug_messages(|| scan_records(&mut h, &mut found, module).unwrap());
            let failures: Vec<String> = messages
                .into_iter()
                .filter(|message| {
                    message.starts_with("invalid serial port")
                        || message.starts_with("Error while instancing")
                        || message.starts_with("error while disconnecting")
                })
                .collect();
            // Once per baud rate.
            assert_eq!(failures, [expected.clone(), expected].concat(), "{module:?}");
        }
    }

    /// Dyadic: the error log of a port that did not open passes the error text as an argument;
    /// the logging error report on standard error takes the place
    /// of the record, before the line of the failed disconnect, once per baud rate.
    #[test]
    fn dyadic_instancing_failure_reports_the_logging_argument_error() {
        use crate::motion::driver_support::testing::{capture_diagnostics, Diagnostic};
        let busy = Device { fail_open: true, ..Device::default() };
        let mut h = host(&[("/dev/ttyUSB3", busy)]);
        let mut found = records(1);
        let diagnostics = capture_diagnostics(|| scan_records(&mut h, &mut found, ScanModule::Dyadic).unwrap());
        let report = Diagnostic::Stderr(
            "--- Logging error ---\n\
             Traceback (most recent call last):\n  \
             File \"backend/controllers/dyadic.py\", line 90, in backend.controllers.dyadic.connect\n  \
             File \"backend/controllers/pytbmscom.py\", line 18, in \
             backend.controllers.pytbmscom.TMBSCOMProtocol.__init__\n  \
             File \"serial\\serialwin32.py\", line 33, in __init__\n  \
             File \"serial\\serialutil.py\", line 244, in __init__\n  \
             File \"serial\\serialwin32.py\", line 64, in open\n\
             serial.serialutil.SerialException: busy\n\
             \n\
             During handling of the above exception, another exception occurred:\n\
             \n\
             Traceback (most recent call last):\n  \
             File \"logging\\__init__.py\", line 1151, in emit\n  \
             File \"logging\\__init__.py\", line 999, in format\n  \
             File \"logging\\__init__.py\", line 712, in format\n  \
             File \"logging\\__init__.py\", line 400, in getMessage\n\
             TypeError: not all arguments converted during string formatting\n\
             Call stack:\n  \
             File \"threading.py\", line 1015, in _bootstrap\n  \
             File \"threading.py\", line 1044, in _bootstrap_inner\n  \
             File \"threading.py\", line 995, in run\n  \
             File \"logging\\__init__.py\", line 1555, in exception\n  \
             File \"logging\\__init__.py\", line 1549, in error\n  \
             File \"logging\\__init__.py\", line 1665, in _log\n  \
             File \"logging\\__init__.py\", line 1681, in handle\n  \
             File \"logging\\__init__.py\", line 1737, in callHandlers\n  \
             File \"logging\\__init__.py\", line 1027, in handle\n  \
             File \"logging\\__init__.py\", line 1265, in emit\n  \
             File \"logging\\__init__.py\", line 1159, in emit\n\
             Message: 'Error while instancing the serial port, will disconnect'\n\
             Arguments: ('busy',)\n"
                .into(),
        );
        let failures: Vec<Diagnostic> = diagnostics
            .into_iter()
            .filter(|diagnostic| match diagnostic {
                Diagnostic::Log(message) => {
                    message.starts_with("Error while instancing") || message.starts_with("error while disconnecting")
                }
                Diagnostic::Stderr(_) => true,
                Diagnostic::Print(_) => false,
            })
            .collect();
        let disconnect = format!(
            "error while disconnecting\n{}{CHAINED}{}",
            open_failure_traceback("dyadic", 90),
            connect_traceback("dyadic", 95, "", UNBOUND)
        );
        let attempt = [report, Diagnostic::log(&disconnect)];
        assert_eq!(failures, [attempt.clone(), attempt].concat());
    }

    #[test]
    fn scan_tries_both_baud_rates_on_other_ftdi_ports() {
        // Each empty slot tries 9600 then 115200 on a port that does not answer.
        let mut h = host(&[("/dev/ttyUSB3", Device::default())]);
        let slots = scan(&mut h, 2, ScanModule::Dyadic);
        assert!(slots.iter().all(|record| record.interface.is_none()));
        let bauds: Vec<u32> = h.log("/dev/ttyUSB3").opens.iter().map(|o| o.baud_rate).collect();
        assert_eq!(bauds, [9600, 115_200, 9600, 115_200]);
    }

    #[test]
    fn scan_skips_non_ftdi_ports() {
        let mut h = FakeHost::with_ports(&["/dev/ttyUSB3"]).device("/dev/ttyUSB3", scn6_device(1, 7));
        let slots = scan(&mut h, 1, ScanModule::Dyadic);
        assert!(slots[0].interface.is_none());
        assert!(h.log("/dev/ttyUSB3").opens.is_empty());
    }

    #[test]
    fn homing_poll_stops_on_completion() {
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(1, 7))]);
        let mut found = scan(&mut h, 1, ScanModule::Dyadic);
        let before = h.now();
        move_org(&mut found[0], &zfill2(7), &mut h.clock, ScanModule::Dyadic).unwrap();
        let messages = capture_debug_messages(|| wait_for_homing(&mut found[0], 0, &mut h.clock, ScanModule::Dyadic));
        assert_eq!(messages, ["homing 0", "homing completed0"]);
        assert!(h.now() - before < HOMING_PAUSE);
        let written = h.log("/dev/ttyUSB3").written();
        assert!(written.iter().any(|w| w == &super::super::tmbs::pack(0, "o0700000000")));
    }

    #[test]
    fn homing_poll_error_policies() {
        // Unanswered STAT reads: Dyadic sleeps 1 s and retries 50 times; the three PRS200 drivers
        // stop at once.
        let mut h = host(&[("/dev/ttyUSB3", scn6_device(1, 7))]);
        let mut found = scan(&mut h, 1, ScanModule::Dyadic);
        let silent: Box<dyn crate::serial::Link> =
            Box::new(crate::motion::driver_support::testing::ScriptedLink::silent("x"));
        found[0].interface = Some(TmbsLink::new(silent));
        let failure = "'int' object is not subscriptable";
        // The protocol's own print for the read that timed out.
        let unanswered =
            "unpackMessage - message size less than expected, looks like its using the wrong baud rate 1 b'\\x02'";
        for module in [ScanModule::Prs200, ScanModule::Prs200Sv, ScanModule::Prs200TlSurge] {
            let t0 = h.now();
            let diagnostics = capture_diagnostics(|| wait_for_homing(&mut found[0], 3, &mut h.clock, module));
            // One unanswered poll: two 0.5 s read timeouts.
            assert!(h.now() - t0 < Duration::from_secs(2));
            let expected = if module == ScanModule::Prs200TlSurge {
                vec![
                    Diagnostic::log("homing 3"),
                    Diagnostic::print(unanswered),
                    Diagnostic::print("readSTATflags - failed"),
                    Diagnostic::log(&format!(
                        "error while homing {failure}\n{}",
                        connect_traceback("prs200_dyadic_tl_surge", 253, "", &format!("TypeError: {failure}"))
                    )),
                ]
            } else {
                vec![
                    Diagnostic::print("homing"),
                    Diagnostic::print(unanswered),
                    Diagnostic::print("readSTATflags - failed"),
                    Diagnostic::print(&format!("error while homing {failure}")),
                ]
            };
            assert_eq!(diagnostics, expected, "{module:?}");
        }
        let t1 = h.now();
        let messages =
            capture_debug_messages(|| wait_for_homing(&mut found[0], 0, &mut h.clock, ScanModule::DyadicPrevious));
        // Fifty polls, each a 1 s timeout plus the 1 s pause.
        assert!(h.now() - t1 >= Duration::from_secs(100));
        assert_eq!(messages.len(), 51);
        let record = format!(
            "error while homing {failure}\n{}",
            connect_traceback("dyadic_previous", 207, "", &format!("TypeError: {failure}"))
        );
        assert!(messages[1..].iter().all(|message| *message == record), "{messages:?}");
    }

    #[test]
    fn homing_calls_on_a_record_without_an_interface() {
        // The record has no interface: the homing movement is an attribute error; inside the poll the same
        // lookup is handled and reported like any other error.
        let mut record = ActuatorRecord::default();
        let mut clock = FakeClock::default();
        let error = move_org(&mut record, "07", &mut clock, ScanModule::Prs200TlSurge).unwrap_err();
        assert!(
            matches!(error.root(), MotionError::Native(detail) if detail == "AttributeError: 'NoneType' object has no attribute 'move_org'")
        );
        let messages =
            capture_debug_messages(|| wait_for_homing(&mut record, 1, &mut clock, ScanModule::Prs200TlSurge));
        let exception = "AttributeError: 'NoneType' object has no attribute 'readSTATflags'";
        let record = format!(
            "error while homing 'NoneType' object has no attribute 'readSTATflags'\n{}",
            connect_traceback("prs200_dyadic_tl_surge", 253, "", exception)
        );
        assert_eq!(messages, ["homing 1".to_string(), record]);
    }

    #[test]
    fn dictionary_text_matches_python() {
        let mut ports = BTreeMap::new();
        assert_eq!(ports_repr(&ports), "{}");
        ports.insert(0, "/dev/ttyUSB3".to_string());
        ports.insert(1, "it's".to_string());
        assert_eq!(ports_repr(&ports), "{0: '/dev/ttyUSB3', 1: \"it's\"}");
        assert_eq!(actuators_repr(&[]), "{}");
        assert_eq!(
            actuators_repr(&records(1)),
            "{0: {'interface': None, 'com_port': '', 'sn': '', 'axis': '', 'speed': -1, 'accel': -1, 'new_speed': None, \
             'new_accel': None}}"
        );
    }

    #[test]
    fn zfill_matches_python() {
        assert_eq!(zfill2(7), "07");
        assert_eq!(zfill2(12), "12");
        assert_eq!(zfill2(123), "123");
        assert_eq!(zfill2(0), "00");
        assert_eq!(zfill2(-1), "-1");
    }

    #[test]
    fn get_id_lookup() {
        // The id: an entry `<axis>,<serial>` that matches gives its key as an integer, else -1.
        let mut a = BTreeMap::new();
        a.insert("2".to_string(), "0,0000ABCD".to_string());
        a.insert("0".to_string(), "0,00001111".to_string());
        a.insert("1".to_string(), "1,0000ABCD,extra".to_string());
        assert_eq!(assigned_id(&a, "0", "0000ABCD").unwrap(), LegacyValue::Int(2));
        assert_eq!(assigned_id(&a, "0", "00001111").unwrap(), LegacyValue::Int(0));
        assert_eq!(
            assigned_id(&a, "1", "0000ABCD").unwrap(),
            LegacyValue::Int(1),
            "parts after the second are ignored"
        );
        assert_eq!(assigned_id(&a, "1", "00001111").unwrap(), LegacyValue::Int(-1));
        for (key, expected) in [("+٢", 2), ("１_２", 12), (" -0_1 ", -1), ("-9223372036854775808", i64::MIN)] {
            let assignment = BTreeMap::from([(key.to_owned(), "0,0000ABCD".to_owned())]);
            assert_eq!(assigned_id(&assignment, "0", "0000ABCD").unwrap(), LegacyValue::Int(expected), "{key:?}");
        }
        // The key has no width limit: a wide key is a valid id (which is later an index error).
        let wide = BTreeMap::from([("9223372036854775808".to_owned(), "0,0000ABCD".to_owned())]);
        assert_eq!(
            assigned_id(&wide, "0", "0000ABCD").unwrap(),
            LegacyValue::from_integer(num_bigint::BigInt::from(1u64 << 63))
        );
        for key in ["1.0", "1__2"] {
            let assignment = BTreeMap::from([(key.to_owned(), "0,0000ABCD".to_owned())]);
            assert!(assigned_id(&assignment, "0", "0000ABCD").is_err(), "{key:?}");
        }
        // The key's conversion is not handled: its value error is returned with its text.
        let assignment = BTreeMap::from([("1.0".to_owned(), "0,0000ABCD".to_owned())]);
        let error = assigned_id(&assignment, "0", "0000ABCD").unwrap_err();
        assert_eq!(error.python_str(), "invalid literal for int() with base 10: '1.0'");
        assert_eq!(error.to_string(), "ValueError: invalid literal for int() with base 10: '1.0'");
    }

    #[test]
    fn an_entry_without_a_comma_ends_the_lookup() {
        // The id, speed and acceleration lookups: an entry without a comma gives -1.
        // The Axis Assignment dialog stores 'None'
        // for an option without an actuator: such an entry before the matching one hides it; after it, it is
        // never reached.
        let unassigned_first =
            BTreeMap::from([("0".to_string(), "None".to_string()), ("1".to_string(), "0,0000ABCD".to_string())]);
        assert_eq!(assigned_key(&unassigned_first, "0", "0000ABCD"), None);
        assert_eq!(assigned_id(&unassigned_first, "0", "0000ABCD").unwrap(), LegacyValue::Int(-1));
        let unassigned_last =
            BTreeMap::from([("0".to_string(), "0,0000ABCD".to_string()), ("1".to_string(), "None".to_string())]);
        assert_eq!(assigned_id(&unassigned_last, "0", "0000ABCD").unwrap(), LegacyValue::Int(0));
        assert_eq!(assigned_id(&unassigned_last, "0", "00001111").unwrap(), LegacyValue::Int(-1));
    }
}

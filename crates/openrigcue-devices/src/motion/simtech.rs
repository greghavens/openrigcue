//! SimTech Revolution actuators (`STR-SCN6150`, `STR-SCN6150-TL`): one TMBSCOM controller per actuator.
//! The driver finds, powers and homes the controllers, then sends positions and keeps each
//! actuator's velocity up to date.

mod tmbscom;

use std::time::Duration;

use tracing::{debug, error};

pub use tmbscom::{StatFlags, TmbsError, TmbsInterface};

use crate::discovery::allowed_search_com_ports;
use crate::motion::clock::Clock;
use crate::motion::native::{self, NativeSetting};
use crate::motion::{DriverFactory, LinkStatus, LiveSettings, MotionContext, MotionDriver, MotionError, PyFrame};
use crate::serial::{ClosedLink, OpenOptions};
use openrigcue_model::native_value::LegacyValue;

/// Baud rates tried per port.
const BAUD_RATES: [u32; 1] = [115_200];
/// The write timeout of a controller's port.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// Smallest accepted model code.
const MIN_MODEL_CODE: i64 = 808_451_712;
/// Power-on and homing polls.
const POWER_ON_POLLS: usize = 50;
const HOMING_POLLS: usize = 10;
const HOMING_POLL_PAUSE: Duration = Duration::from_secs(1);
/// Every controller drives axis 0.
const DEFAULT_AXIS: u32 = 0;
/// Homing pattern whose actuators take `-value` instead of the maximum minus the value.
const REVERSED_HOMING_PATTERN: i64 = 7;
/// Assignment slot of the traction-loss actuator and its rescaled span.
const TL_SLOT: usize = 4;
const TL_SPAN: f64 = 20_000.0;
/// A velocity write sends its command 11 times.
const VELOCITY_REPEATS: usize = 11;
/// The refresh rate is capped at 30 off Windows.
const NON_WINDOWS_MAX_RATE: i64 = 30;
/// The seconds since homing wrap at one day.
const SECONDS_PER_DAY: u64 = 86_400;

/// The serial number text: eight upper-case hex digits of the low 32 bits.
pub fn convert_sn(sn: i64) -> String {
    (0..8).rev().map(|i| char::from(b"0123456789ABCDEF"[((sn >> (4 * i)) & 15) as usize])).collect()
}

/// One actuator record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActuatorRecord {
    pub com_port: String,
    pub sn: String,
    pub axis: String,
    /// `-1` until the first velocity was written.
    pub speed: i64,
    pub accel: i64,
    pub homing_pattern: Option<i64>,
    pub baud_rate: Option<u32>,
}

impl Default for ActuatorRecord {
    fn default() -> Self {
        ActuatorRecord {
            com_port: String::new(),
            sn: String::new(),
            axis: String::new(),
            speed: -1,
            accel: -1,
            homing_pattern: None,
            baud_rate: None,
        }
    }
}

/// What the UI reads back: the actuator records and the axis assignment status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimTechShared {
    pub actuators: Vec<ActuatorRecord>,
    pub axis_assignment_status: bool,
}

/// What the driver reads. Required lookup/conversion failures are deferred to their consumer reads.
#[derive(Debug, Clone, PartialEq)]
pub struct SimTechSettings {
    /// Live state for the steps that read it.
    pub runtime: crate::motion::driver_support::RuntimeReader,
    /// Whether the motion switch is on.
    pub motion_switch_on: bool,
    /// Whether a motion error is set.
    pub motion_error: bool,
    /// `[SIMTECHREVOLUTION] post_homing_speed`, `post_homing_acceleration`, `post_homing_speed_timeout`.
    pub post_homing_speed: NativeSetting,
    pub post_homing_acceleration: NativeSetting,
    pub post_homing_speed_timeout: NativeSetting,
    /// The tune's `scn6_speed` and `scn6_acceleration`.
    pub scn6_speed: NativeSetting,
    pub scn6_acceleration: NativeSetting,
    /// The axis assignment: assignment slot → `'axis,sn'`, in insertion order.
    pub axis_assignment: Vec<(String, String)>,
    /// `[MOTIONBOX] refresh_rate`.
    pub refresh_rate: NativeSetting,
    /// Whether the platform is Windows (the 30 Hz cap of the refresh rate applies only when false).
    pub windows: bool,
}

impl Default for SimTechSettings {
    fn default() -> Self {
        let missing = |key: &str| Err(format!("KeyError: '{key}'"));
        Self {
            runtime: Default::default(),
            motion_switch_on: false,
            motion_error: false,
            post_homing_speed: missing("post_homing_speed"),
            post_homing_acceleration: missing("post_homing_acceleration"),
            post_homing_speed_timeout: missing("post_homing_speed_timeout"),
            scn6_speed: missing("scn6_speed"),
            scn6_acceleration: missing("scn6_acceleration"),
            axis_assignment: Vec::new(),
            refresh_rate: missing("refresh_rate"),
            windows: false,
        }
    }
}

/// A raw setting as an integer, at any width.
fn required(value: &NativeSetting) -> Result<LegacyValue, MotionError> {
    native::to_int(&value.clone().map_err(MotionError::Native)?)
}

/// The file name in the frames an error of this driver passes.
const SIMTECH_FILE: &str = "backend/controllers/simtechrevolution.py";

/// The frame for `function` and `line` in this driver's file.
const fn at(function: &'static str, line: u32) -> PyFrame {
    PyFrame::new(SIMTECH_FILE, function, line)
}

/// A setting read as an integer at `frame`.
fn required_at(value: &NativeSetting, frame: PyFrame) -> Result<LegacyValue, MotionError> {
    required(value).map_err(|e| e.in_frame(frame))
}

/// The frame name and lines of the speed or acceleration lookup: the time since homing, the timeout read, the
/// post-homing value and the tune's value.
struct VelocityLines {
    function: &'static str,
    elapsed: u32,
    timeout: u32,
    post_homing: u32,
    game: u32,
    /// The `SIMTECHREVOLUTION` key of the post-homing value and the tune's key.
    post_key: &'static str,
    game_key: &'static str,
}

const GET_SPEED: VelocityLines = VelocityLines {
    function: "get_speed",
    elapsed: 412,
    timeout: 413,
    post_homing: 416,
    game: 420,
    post_key: "post_homing_speed",
    game_key: "scn6_speed",
};
const GET_ACCEL: VelocityLines = VelocityLines {
    function: "get_accel",
    elapsed: 428,
    timeout: 429,
    post_homing: 432,
    game: 436,
    post_key: "post_homing_acceleration",
    game_key: "scn6_acceleration",
};

/// The error of a protocol call: a reply that is not hexadecimal is a value error
/// with the protocol frames it passes.
fn tmbs_error(e: TmbsError) -> MotionError {
    match e {
        TmbsError::Parse(error) => super::scn6::axis::uncaught(error),
        TmbsError::Raised(detail) => MotionError::Native(detail),
        other => MotionError::Protocol(other.to_string()),
    }
}

/// The assignment slot whose first two `','` parts are `axis` and `sn`,
/// else `-1`. A value with fewer than two parts gives `-1` at once; extra parts
/// are ignored. A matching key that is not an integer is an error.
pub fn assignment_slot(assignment: &[(String, String)], axis: &str, sn: &str) -> Result<LegacyValue, MotionError> {
    for (key, value) in assignment {
        let mut parts = value.split(',');
        let (Some(a), Some(s)) = (parts.next(), parts.next()) else {
            return Ok(native::int(-1));
        };
        if a == axis && s == sn {
            // The key as an integer, at any width.
            return native::to_int(&LegacyValue::Str(key.clone())).map_err(|e| e.in_frame(at("get_id", 403)));
        }
    }
    Ok(native::int(-1))
}

/// Whether the hardware id contains `0403:60`: FTDI with a 0x60xx product id.
fn is_ftdi_60(port: &crate::discovery::ListPortInfo) -> Result<bool, MotionError> {
    port.hwid_contains("0403:60").map_err(|message| MotionError::Native(format!("TypeError: {message}")))
}

const SEND_ACTUATOR_POS: &str = "send_actuator_pos";
/// The frame line of the slot read in the position pass, before the traction-loss rescale and the homing-pattern test.
const MOTOR_SUBSCRIPT_LINE: u32 = 520;

/// Why one port attempt of a connect failed: the port did not open (which is handled),
/// or a later step failed, which ends the connect.
enum ProbeFailure {
    Open(crate::serial::LinkError),
    Raised(MotionError),
}

/// The frame line of the opening of the protocol on a port in the connect.
const INSTANCE_LINE: u32 = 240;
/// The frame line of the disconnect after a failed open.
const UNBOUND_DISCONNECT_LINE: u32 = 246;
/// The frame lines of the three steps of the connect whose errors are logged: the model code read,
/// the "Main power ON" flag read of the power-on poll and
/// the "Homing completion status" flag read of the homing poll.
const MODEL_CODE_LINE: u32 = 257;
const POWER_POLL_LINE: u32 = 305;
const HOMING_POLL_LINE: u32 = 368;

/// The error of a STAT flag poll at `line`: a type error for an
/// unanswered read, else the read's own.
fn poll_error(result: Result<Option<StatFlags>, TmbsError>, line: u32) -> Result<StatFlags, MotionError> {
    match result {
        Ok(Some(flags)) => Ok(flags),
        Ok(None) => {
            Err(MotionError::Native("TypeError: 'int' object is not subscriptable".into())
                .in_frame(at("connect", line)))
        }
        Err(e) => Err(tmbs_error(e).in_frame(at("connect", line))),
    }
}

/// The frame lines of the steps after the model code check whose protocol errors end the connect:
/// the ALRM flags read, the four alarm commands, servo on, the serial number read
/// and the homing pattern read.
const ALARM_LINE: u32 = 293;
const CANCEL_LINE: u32 = 295;
const RESET_LINE: u32 = 296;
const CHECK_LINE: u32 = 297;
const SERVO_OFF_LINE: u32 = 298;
const SERVO_ON_LINE: u32 = 301;
const SERIAL_LINE: u32 = 331;
const HOMING_PATTERN_LINE: u32 = 335;

/// The error of a protocol call of the connect at `line`.
fn raised_at(line: u32) -> impl Fn(TmbsError) -> MotionError {
    move |e| tmbs_error(e).through(at("connect", line))
}

/// The SimTech Revolution driver.
pub struct SimTechDriver {
    settings: LiveSettings<SimTechSettings>,
    shared: LiveSettings<SimTechShared>,
    actuators: Vec<ActuatorRecord>,
    /// The speed and acceleration of each record as dynamic integers.
    velocity: Vec<[LegacyValue; 2]>,
    interfaces: Vec<Option<TmbsInterface>>,
    /// The ports in use during a connect: actuator index → the port it claimed once powered on, whether or not the
    /// rest of its probe succeeded.
    ports_in_use: Vec<Option<String>>,
    /// When homing completed (clock time).
    homing_completed: Option<Duration>,
}

impl SimTechDriver {
    pub fn new(settings: LiveSettings<SimTechSettings>, shared: LiveSettings<SimTechShared>) -> Self {
        SimTechDriver {
            settings,
            shared,
            actuators: Vec::new(),
            velocity: Vec::new(),
            interfaces: Vec::new(),
            ports_in_use: Vec::new(),
            homing_completed: None,
        }
    }

    pub fn factory(settings: LiveSettings<SimTechSettings>, shared: LiveSettings<SimTechShared>) -> DriverFactory {
        Box::new(move || Box::new(SimTechDriver::new(settings.clone(), shared.clone())))
    }

    fn publish(&self) {
        let actuators = self.actuators.clone();
        self.shared.update(|s| s.actuators = actuators);
    }

    fn set_assignment_status(&self, ok: bool) {
        self.shared.update(|s| s.axis_assignment_status = ok);
    }

    /// Open and identify one port for actuator `idx`. `Ok(false)` when the port is not
    /// a SimTech controller (its model code is 0, too low, or its read failed); `ProbeFailure::Open` when the
    /// port does not open; `ProbeFailure::Raised` for a later step, whose error
    /// ends the connect. The port is claimed as in use right after
    /// power-on, the record holds the interface from `set iterface` on and is filled field by field (`axis`,
    /// `sn`, `homing_pattern`, then `com_port` and `baud_rate`), so a failed read leaves the fields read so
    /// far, the interface and the claim in place.
    fn probe(
        &mut self,
        idx: usize,
        port: &str,
        baud_rate: u32,
        ctx: &mut MotionContext<'_>,
    ) -> Result<bool, ProbeFailure> {
        debug!("Trying to instance {port} baud_rate: {baud_rate}");
        let link = ctx
            .host
            .open_serial(port, &OpenOptions { baud_rate, write_timeout: Some(WRITE_TIMEOUT) })
            .map_err(ProbeFailure::Open)?;
        self.identify_on(idx, port, baud_rate, TmbsInterface::new(link), ctx).map_err(ProbeFailure::Raised)
    }

    /// The part of the probe after the port opened.
    fn identify_on(
        &mut self,
        idx: usize,
        port: &str,
        baud_rate: u32,
        mut iface: TmbsInterface,
        ctx: &mut MotionContext<'_>,
    ) -> Result<bool, MotionError> {
        let prefix = format!("{port} {baud_rate}");
        let clock = ctx.host.clock();
        debug!("{prefix} Time to check the protocol");
        let model_code = match iface.read_model_code(clock) {
            Ok(model_code) => model_code,
            Err(e) => {
                // The error is logged with the port and the read's traceback, then the interface is
                // disconnected (dropping the
                // interface closes its port) and the next attempt follows, as for a model code of 0.
                let raised = tmbs_error(e).in_frame(at("connect", MODEL_CODE_LINE));
                error!(
                    "{}",
                    crate::motion::driver_support::exception_record(
                        &format!("{port}error while reading model code not simtechrevolution"),
                        &raised,
                        None
                    )
                );
                return Ok(false);
            }
        };
        debug!("{prefix} model_code: {model_code}");
        if model_code == 0 {
            debug!("{prefix} error while reading model code not simtechrevolution");
            return Ok(false);
        }
        if model_code < MIN_MODEL_CODE {
            debug!("{prefix} model code is not what I was expecting: {model_code}");
            return Ok(false);
        }
        debug!("{prefix} correct model");
        if iface.read_alarm_flags(clock).map_err(raised_at(ALARM_LINE))? != 0 {
            debug!("{prefix} there is an alarm, trying to reset");
            iface.cancel_movement(clock).map_err(raised_at(CANCEL_LINE))?;
            iface.reset_alarm(clock).map_err(raised_at(RESET_LINE))?;
            iface.check_alarm(clock).map_err(raised_at(CHECK_LINE))?;
            iface.set_servo_off(clock).map_err(raised_at(SERVO_OFF_LINE))?;
        }
        debug!("{prefix} sending set_son");
        iface.set_servo_on(clock).map_err(raised_at(SERVO_ON_LINE))?;
        Self::wait_for_power(&mut iface, baud_rate, clock);
        debug!("{prefix} set iterface");
        // The record holds the interface from here on (an earlier object it held is dropped, closing its port).
        let iface = self.interfaces[idx].insert(iface);
        debug!("{prefix} set port no");
        self.ports_in_use[idx] = Some(port.to_string());
        let record = &mut self.actuators[idx];
        debug!("{prefix} set default axis");
        record.axis = DEFAULT_AXIS.to_string();
        debug!("{prefix} set sn {idx}");
        debug!("working on the key: {idx} port: {port} baud_rate: {baud_rate}");
        record.sn = convert_sn(iface.read_serial_number(clock).map_err(raised_at(SERIAL_LINE))?);
        debug!("{prefix} read homing pattern");
        let homing_pattern = iface.read_homing_pattern(clock).map_err(raised_at(HOMING_PATTERN_LINE))?;
        record.homing_pattern = Some(homing_pattern);
        debug!("{prefix} homing pattern: {homing_pattern}");
        debug!("{prefix} set comport");
        record.com_port = port.to_string();
        record.baud_rate = Some(baud_rate);
        Ok(true)
    }

    /// The power-on poll: up to 50 STAT reads until `Main power ON`; failures are logged and retried.
    fn wait_for_power(iface: &mut TmbsInterface, baud_rate: u32, clock: &mut dyn Clock) {
        for _ in 0..POWER_ON_POLLS {
            match poll_error(iface.read_stat_flags(clock), POWER_POLL_LINE) {
                Ok(flags) if flags.main_power_on => {
                    debug!("{} {baud_rate} seton is done", iface.port());
                    return;
                }
                Ok(_) => debug!("{} {baud_rate} turning on", iface.port()),
                // The error is logged with the port and its text ("error while powering on").
                Err(raised) => error!(
                    "{}",
                    crate::motion::driver_support::exception_record(
                        &format!("{}error while powering on {}", iface.port(), raised.python_str()),
                        &raised,
                        None
                    )
                ),
            }
        }
    }

    /// The homing step after every actuator was found: the homing movement with the zero-filled homing pattern, then
    /// up to 10 polls of `Homing completion status`, 1 s apart.
    fn home_all(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        for (iface, record) in self.interfaces.iter_mut().zip(&self.actuators) {
            let Some(iface) = iface else { continue };
            let pattern = format!("{:02}", record.homing_pattern.unwrap_or_default());
            debug!("{} seting move org to {pattern}", record_prefix(record));
            // The homing movement; its error is not handled.
            iface
                .move_org(DEFAULT_AXIS, &pattern, ctx.host.clock())
                .map_err(|e| tmbs_error(e).through(at("connect", 361)))?;
        }
        for (idx, iface) in self.interfaces.iter_mut().enumerate() {
            let Some(iface) = iface else { continue };
            let prefix = record_prefix(&self.actuators[idx]);
            for _ in 0..HOMING_POLLS {
                debug!("{prefix} homing {idx}");
                match poll_error(iface.read_stat_flags(ctx.host.clock()), HOMING_POLL_LINE) {
                    Ok(flags) if flags.homing_complete => {
                        debug!("{prefix} homing completed {idx}");
                        break;
                    }
                    Ok(_) => debug!("{prefix} homing {idx}"),
                    // The error is logged with its text ("error while homing").
                    Err(raised) => error!(
                        "{}",
                        crate::motion::driver_support::exception_record(
                            &format!("{prefix} error while homing {}", raised.python_str()),
                            &raised,
                            None
                        )
                    ),
                }
                ctx.host.clock().sleep(HOMING_POLL_PAUSE);
            }
        }
        Ok(())
    }

    /// The speed or the acceleration (by the keys of `lines`): the post-homing value
    /// during `post_homing_speed_timeout` seconds after homing, else the tune's value; every integer at any width.
    fn velocity_setting(
        &self,
        post: &NativeSetting,
        game: &NativeSetting,
        lines: &VelocityLines,
        clock: &dyn Clock,
    ) -> Result<LegacyValue, MotionError> {
        let settings = self.settings.get();
        let line = |line: u32| at(lines.function, line);
        // The homing time exists only once a connect homed the actuators.
        let homed = self.homing_completed.ok_or_else(|| {
            MotionError::Native("NameError: name 'homing_completed' is not defined".into())
                .in_frame(line(lines.elapsed))
        })?;
        let seconds = clock.now().saturating_sub(homed).as_secs() % SECONDS_PER_DAY;
        // Every read is the setting's or the tune's value as it is now.
        let timeout = settings.runtime.value(
            "SIMTECHREVOLUTION",
            "post_homing_speed_timeout",
            &settings.post_homing_speed_timeout,
        );
        let timeout = required_at(&timeout, line(lines.timeout))?;
        let within = native::lt(&native::big(seconds.into()), &timeout).map_err(|e| e.in_frame(line(lines.timeout)))?;
        if within {
            required_at(&settings.runtime.value("SIMTECHREVOLUTION", lines.post_key, post), line(lines.post_homing))
        } else {
            required_at(&settings.runtime.game_value(lines.game_key, game), line(lines.game))
        }
    }

    fn speed_now(&self, clock: &dyn Clock) -> Result<LegacyValue, MotionError> {
        let s = self.settings.get();
        self.velocity_setting(&s.post_homing_speed, &s.scn6_speed, &GET_SPEED, clock)
    }

    fn accel_now(&self, clock: &dyn Clock) -> Result<LegacyValue, MotionError> {
        let s = self.settings.get();
        self.velocity_setting(&s.post_homing_acceleration, &s.scn6_acceleration, &GET_ACCEL, clock)
    }

    /// The pause before each velocity command: one refresh period. The rate is read as an
    /// integer, capped at 30 off Windows, and the period is `1 / rate` (a float of any
    /// integer); a period that cannot be slept is an error.
    fn velocity_pause(&self) -> Result<Duration, MotionError> {
        let settings = self.settings.get();
        let sleep = at("write_velocity", 142);
        let in_get_refresh_rate = |line: u32| [sleep, at("get_refresh_rate", line)];
        let mut rate = required(&settings.runtime.value("MOTIONBOX", "refresh_rate", &settings.refresh_rate))
            .map_err(|e| e.in_frames(in_get_refresh_rate(120)))?;
        let cap = native::int(NON_WINDOWS_MAX_RATE);
        // Two integers: the comparison cannot fail.
        if !settings.windows && native::gt(&rate, &cap).expect("int comparison") {
            rate = cap;
        }
        let period = native::truediv(&native::int(1), &rate).map_err(|e| e.in_frames(in_get_refresh_rate(125)))?;
        let seconds = native::double(&period).map_err(|e| e.in_frame(sleep))?;
        openrigcue_model::python_time::sleep_duration(seconds).map_err(|message| {
            let class = if message.starts_with("sleep length") { "ValueError" } else { "OverflowError" };
            MotionError::Native(format!("{class}: {message}")).in_frame(sleep)
        })
    }

    /// Writes an actuator's velocity: the velocity command 11 times, each after one
    /// refresh period; stores the values unless a write failed.
    fn write_velocity(
        &mut self,
        idx: usize,
        speed: LegacyValue,
        accel: LegacyValue,
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        // The new velocity is logged with the port; each value is converted to text first.
        let (speed_text, accel_text) = (native::str_checked(&speed)?, native::str_checked(&accel)?);
        debug!("setting actuator new velocity {} {speed_text} {accel_text}", self.actuators[idx].com_port);
        let axis = crate::motion::thanos::protocol::parse_python_int(&self.actuators[idx].axis);
        for _ in 0..VELOCITY_REPEATS {
            ctx.host.clock().sleep(self.velocity_pause()?);
            let (Some(iface), Some(axis)) = (self.interfaces.get_mut(idx).and_then(Option::as_mut), axis) else {
                debug!("error");
                return Ok(());
            };
            match iface.write_velocity_native(axis as u32, &speed, &accel, ctx.host.clock()) {
                Ok(status) => debug!(
                    "status of the velocity sent the actuator {}  velocity:   {}",
                    self.actuators[idx].com_port,
                    if status { "True" } else { "False" }
                ),
                Err(_) => {
                    debug!("error");
                    return Ok(());
                }
            }
        }
        self.store_velocity(idx, speed, accel);
        Ok(())
    }

    /// Stores an actuator's speed and acceleration: the dynamic values drive every later test; the shared
    /// record (whose fields the SCN6 family keeps as `i64`) takes a value that fits one.
    fn store_velocity(&mut self, idx: usize, speed: LegacyValue, accel: LegacyValue) {
        if let Some(value) = native::as_i64(&speed) {
            self.actuators[idx].speed = value;
        }
        if let Some(value) = native::as_i64(&accel) {
            self.actuators[idx].accel = value;
        }
        self.velocity[idx] = [speed, accel];
    }

    /// The velocity part of the position pass for one actuator. A first value is the tune's non-zero
    /// `scn6_speed` / `scn6_acceleration`, else the current speed / acceleration; later frames rewrite the
    /// velocity when the current speed or acceleration differ from the stored values. `Ok(false)` when the
    /// actuator has no value, which makes the assignment status false and ends the pass.
    fn update_velocity(&mut self, idx: usize, ctx: &mut MotionContext<'_>) -> Result<bool, MotionError> {
        let settings = self.settings.get();
        // The frame line of the position pass an error of a called step passes.
        let called_at = |line: u32| move |e: MotionError| e.through(at(SEND_ACTUATOR_POS, line));
        let unset = |value: &LegacyValue| native::eq(value, &native::int(-1));
        let [speed, accel] = self.velocity[idx].clone();
        let first_time = unset(&speed)? || unset(&accel)?;
        if unset(&self.velocity[idx][0])? {
            // The tune's speed as an integer; when it is 0, the current speed.
            let game = required_at(
                &settings.runtime.game_value("scn6_speed", &settings.scn6_speed),
                at(SEND_ACTUATOR_POS, 466),
            )?;
            let value = if !native::eq(&game, &native::int(0))? {
                game
            } else {
                self.speed_now(ctx.host.clock()).map_err(called_at(469))?
            };
            self.store_speed(idx, value);
            if unset(&self.velocity[idx][0])? {
                return Ok(false);
            }
        }
        if unset(&self.velocity[idx][1])? {
            // The tune's acceleration as an integer; when it is 0, the current acceleration.
            let game = required_at(
                &settings.runtime.game_value("scn6_acceleration", &settings.scn6_acceleration),
                at(SEND_ACTUATOR_POS, 479),
            )?;
            let value = if !native::eq(&game, &native::int(0))? {
                game
            } else {
                self.accel_now(ctx.host.clock()).map_err(called_at(482))?
            };
            self.store_accel(idx, value);
            if unset(&self.velocity[idx][1])? {
                return Ok(false);
            }
        }
        let [speed, accel] = self.velocity[idx].clone();
        if first_time {
            let (speed_text, accel_text) = (native::str_checked(&speed)?, native::str_checked(&accel)?);
            debug!("setting initial velocity to {speed_text} {accel_text}");
            // The first velocity write.
            self.write_velocity(idx, speed, accel, ctx).map_err(called_at(492))?;
        } else {
            // The current speed, then the current acceleration.
            let new_speed = self.speed_now(ctx.host.clock()).map_err(called_at(494))?;
            let new_accel = self.accel_now(ctx.host.clock()).map_err(called_at(495))?;
            if !native::eq(&new_accel, &accel)? || !native::eq(&new_speed, &speed)? {
                let (speed_text, accel_text) = (native::str_checked(&new_speed)?, native::str_checked(&new_accel)?);
                debug!("{} setting velocity to {speed_text} {accel_text}", record_prefix(&self.actuators[idx]));
                self.write_velocity(idx, new_speed, new_accel, ctx).map_err(called_at(500))?;
            }
        }
        Ok(true)
    }

    fn store_speed(&mut self, idx: usize, value: LegacyValue) {
        let accel = self.velocity[idx][1].clone();
        let speed_mirror = native::as_i64(&value);
        self.velocity[idx] = [value, accel];
        if let Some(mirror) = speed_mirror {
            self.actuators[idx].speed = mirror;
        }
    }

    fn store_accel(&mut self, idx: usize, value: LegacyValue) {
        let speed = self.velocity[idx][0].clone();
        let accel_mirror = native::as_i64(&value);
        self.velocity[idx] = [speed, value];
        if let Some(mirror) = accel_mirror {
            self.actuators[idx].accel = mirror;
        }
    }

    /// The position pass: each actuator's velocity, then its position from the motor list.
    fn send_actuator_pos(&mut self, motors: &LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let assignment = self.settings.get().axis_assignment;
        let mut slots = Vec::with_capacity(self.actuators.len());
        for idx in 0..self.actuators.len() {
            let velocity_ok = self.update_velocity(idx, ctx);
            self.publish();
            if !velocity_ok? {
                self.set_assignment_status(false);
                return Ok(());
            }
            let record = &self.actuators[idx];
            // The assignment slot of the actuator.
            let slot = assignment_slot(&assignment, &record.axis, &record.sn)
                .map_err(|e| e.through(at(SEND_ACTUATOR_POS, 502)))?;
            if native::eq(&slot, &native::int(-1))? {
                self.set_assignment_status(false);
                return Ok(());
            }
            slots.push(slot);
        }
        self.set_assignment_status(true);
        let max = ctx.model.max_motor_abs;
        for (idx, slot) in slots.into_iter().enumerate() {
            // The slot of the id: a negative id (an assignment key edited by hand; -1 has already ended the
            // frame) indexes from the end of the list; one outside the list is
            // an index error there.
            let raw =
                native::item_at(motors, &slot).map_err(|e| e.in_frame(at(SEND_ACTUATOR_POS, MOTOR_SUBSCRIPT_LINE)))?;
            let value = if native::eq(&slot, &native::int(TL_SLOT as i64))? {
                // The same frame line rescales the TL slot.
                let mapped =
                    native::map([&raw, &native::int(0), &native::int(max), &native::int(0), &native::float(TL_SPAN)])
                        .map_err(|e| e.in_frame(at(SEND_ACTUATOR_POS, MOTOR_SUBSCRIPT_LINE)))?;
                native::trunc_float(mapped)?
            } else {
                raw
            };
            let position = if self.actuators[idx].homing_pattern == Some(REVERSED_HOMING_PATTERN) {
                native::mul(&value, &native::int(-1))?
            } else {
                native::sub(&native::int(max), &value)?
            };
            let position = i64::from(i32::from_be_bytes(
                native::pack_i32(&position, true)
                    .map_err(|e| e.in_frame(crate::motion::scn6::tmbs::MOVE_ABS_INTEGER_PACK))?,
            ));
            let iface = self
                .interfaces
                .get_mut(idx)
                .and_then(Option::as_mut)
                .ok_or_else(|| MotionError::Protocol("actuator not connected".into()))?;
            iface.move_absolute(DEFAULT_AXIS, position).map_err(tmbs_error)?;
        }
        Ok(())
    }
}

impl MotionDriver for SimTechDriver {
    /// Connects: find one SimTech controller per actuator, reset alarms, power on, home every
    /// actuator, then `Connected`.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        debug!("trying to connect");
        let settings = self.settings.get();
        let (switch_on, motion_error) = match settings.runtime.read() {
            Some(view) => (view.motion_switch, view.motion_error),
            None => (settings.motion_switch_on, settings.motion_error),
        };
        if !switch_on || motion_error {
            return Ok(());
        }
        debug!("pass checks: switch is on, no motion errors");
        let count = ctx.model.controllers;
        debug!("nbr_controllers that I need to find {count}");
        debug!("disconnect from all controllers");
        debug!("create actuator_dict");
        self.actuators = vec![ActuatorRecord::default(); count];
        self.velocity = vec![[native::int(-1), native::int(-1)]; count];
        self.interfaces = (0..count).map(|_| None).collect();
        self.ports_in_use = vec![None; count];
        self.publish();
        debug!("list serial ports");
        let ports = ctx.host.serial_ports_checked()?;
        let allowed = ctx.host.allowed_com_ports();
        // The actuator whose interface is the current one: the open-failure handling
        // disconnects it.
        let mut interface_idx: Option<usize> = None;
        // Whether an interface was ever opened: before that, the disconnect of the open-failure handling
        // is itself an error.
        let mut interface_bound = false;
        for idx in 0..count {
            debug!("Trying to find com_port of controller {idx}");
            for info in &ports {
                let port = info.path.as_str();
                if !allowed_search_com_ports(port, &allowed) {
                    debug!("{port} is NOT in the allowed ports list");
                    continue;
                }
                let details = ctx.host.port_details(info);
                debug!("Checking port {port} {} {}", details.description, details.hwid_text());
                for baud_rate in BAUD_RATES {
                    let taken = self.ports_in_use.iter().flatten().any(|p| p == port);
                    if !self.actuators[idx].com_port.is_empty() {
                        continue;
                    }
                    debug!("Com port is empty, will look for serial ports");
                    let used = openrigcue_model::native_value::LegacyValue::List(
                        self.ports_in_use.iter().flatten().cloned().map(Into::into).collect(),
                    );
                    debug!(
                        "Trying to instance {port} baud_rate: {baud_rate} {} {} dict_values({})",
                        details.description,
                        details.hwid_text(),
                        openrigcue_model::python_repr::repr(&used),
                    );
                    debug!("{port} {baud_rate}  is in the allowed ports list");
                    if taken {
                        continue;
                    }
                    debug!("{port} {baud_rate}  is not being used by another controller");
                    // The hardware id is tested for `0403:60`.
                    if !is_ftdi_60(&details).map_err(|e| e.in_frame(at("connect", 232)))? {
                        continue;
                    }
                    debug!("{port} {baud_rate}  The address is valid");
                    match self.probe(idx, port, baud_rate, ctx) {
                        Ok(true) => {
                            debug!("done with {idx}");
                            interface_idx = Some(idx);
                            interface_bound = true;
                        }
                        Ok(false) => {
                            interface_idx = None;
                            interface_bound = true;
                        }
                        Err(ProbeFailure::Open(e)) => {
                            // The error is logged with its text as an argument. The message has no
                            // placeholder for its argument, so no record
                            // is written; the failed formatting is reported on standard error.
                            let detail = crate::motion::python::link_error_str(&e);
                            let opened = crate::motion::tmbs::open_exception(e, at("connect", INSTANCE_LINE));
                            crate::motion::driver_support::connect_logging_argument_error(
                                &format!("{port}Error while instancing the serial port, will disconnect"),
                                &[detail],
                                &opened,
                            );
                            // The port did not open: the current interface is still the previous one.
                            if let Some(iface) = interface_idx.and_then(|i| self.interfaces[i].as_mut()) {
                                let port = iface.port().to_string();
                                *iface = TmbsInterface::new(Box::new(ClosedLink::new(&port)));
                            } else if !interface_bound {
                                // The disconnect fails while the open failure is being handled;
                                // it is logged with the port ("error while disconnecting").
                                let unbound = crate::motion::driver_support::unbound_interface(at(
                                    "connect",
                                    UNBOUND_DISCONNECT_LINE,
                                ));
                                error!(
                                    "{}",
                                    crate::motion::driver_support::exception_record(
                                        &format!("{port}error while disconnecting"),
                                        &unbound,
                                        Some(&opened)
                                    )
                                );
                            }
                        }
                        Err(ProbeFailure::Raised(e)) => {
                            // The error is not handled: it ends the connect, with the records,
                            // interfaces and claims as far as they were assigned.
                            self.publish();
                            return Err(e);
                        }
                    }
                }
            }
        }
        self.publish();
        let all_connected = self.actuators.iter().all(|a| !a.com_port.is_empty());
        debug!("all_ports connected {}", if all_connected { "True" } else { "False" });
        if !all_connected {
            debug!("setting motion status as NOT Connected");
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
            return Ok(());
        }
        self.home_all(ctx)?;
        debug!("setting motion status as Connected");
        ctx.link.status = LinkStatus::Connected;
        self.homing_completed = Some(ctx.host.clock().now());
        debug!("Setting the actuator com ports");
        for (idx, record) in self.actuators.iter().enumerate() {
            ctx.link.ports.insert(idx, record.com_port.clone());
        }
        Ok(())
    }

    /// Disconnects: forgets every actuator (the ports close as the interfaces are dropped).
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.actuators.clear();
        self.interfaces.clear();
        self.publish();
        Ok(())
    }

    /// A stream frame: the position pass.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_stream_native(&mut native::list_of(motors), ctx)
    }

    /// A stream frame on the caller's list (which is only read).
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_actuator_pos(motors, ctx).map_err(|e| e.through(at("send_stream", 539)))
    }

    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_actuator_pos(motors, ctx).map_err(|e| e.through(at("send_test", 543)))
    }

    /// A test frame: the position pass.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_test_native(&mut native::list_of(motors), ctx)
    }

    fn describe(&self) -> String {
        format!("SimTech Revolution, {} actuator(s)", self.actuators.len())
    }
}

fn record_prefix(record: &ActuatorRecord) -> String {
    format!("{} {}", record.com_port, record.baud_rate.map(|rate| rate.to_string()).unwrap_or_else(|| "None".into()))
}

#[cfg(test)]
mod tests {
    use super::tmbscom::{
        bcc, pack, read_register_command, velocity_command, ALRM_REGISTER, HOMING_PATTERN_REGISTER,
        MODEL_CODE_REGISTER, SERIAL_NUMBER_REGISTER, SERVO_ON, STAT_REGISTER,
    };
    use super::*;
    use crate::discovery::UsbIdentity;
    use crate::motion::testing::{Device, FakeHost};
    use crate::motion::{motion_model, LinkState, MotionHost};

    /// A reply frame: STX, `U`, axis `0`, payload, BCC, ETX.
    fn reply(payload: &str) -> Vec<u8> {
        let body = format!("U0{payload}");
        let mut out = vec![0x02];
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(bcc(body.as_bytes()).as_bytes());
        out.push(0x03);
        out
    }

    /// A scripted controller: model code 0x808976450, homing pattern 7,
    /// STAT 0x0B (power on, homing done), no alarm, `R400000000` to every action.
    fn controller(serial: u32, model_payload: &str) -> Device {
        let register = |addr: &str| pack(&read_register_command(addr), 0);
        let mut replies = vec![
            (register(MODEL_CODE_REGISTER), reply(model_payload)),
            (register(SERIAL_NUMBER_REGISTER), reply(&format!("R4{serial:08X}"))),
            (register(HOMING_PATTERN_REGISTER), reply("R400000007")),
            (register(STAT_REGISTER), reply("R40000000B")),
            (register(ALRM_REGISTER), reply("OK00000000")),
            (pack(SERVO_ON, 0), reply("R400000000")),
            (pack("o0700000000", 0), reply("R400000000")),
        ];
        for (v, a) in [(800, 100), (10000, 80)] {
            replies.push((pack(&velocity_command(v, a), 0), reply("R400000000")));
        }
        Device { replies, ..Device::default() }
    }

    fn ftdi(path: &str) -> crate::discovery::PortInfo {
        crate::discovery::PortInfo {
            path: path.into(),
            stable_path: None,
            usb: Some(UsbIdentity {
                vendor_id: 0x0403,
                product_id: 0x6001,
                serial_number: None,
                manufacturer: None,
                product: None,
            }),
        }
    }

    struct Rig {
        driver: SimTechDriver,
        settings: LiveSettings<SimTechSettings>,
        shared: LiveSettings<SimTechShared>,
        host: FakeHost,
        link: LinkState,
        model: &'static str,
    }

    impl Rig {
        fn new(model: &'static str, actuators: u32) -> Self {
            let mut host = FakeHost::default();
            for i in 0..actuators {
                let path = format!("/dev/ttyUSB{i}");
                host.ports.push(ftdi(&path));
                host.devices.insert(path, controller(i + 1, "R4808976450"));
            }
            let settings = LiveSettings::new(SimTechSettings {
                runtime: Default::default(),
                motion_switch_on: true,
                motion_error: false,
                post_homing_speed: Ok(LegacyValue::Int(800)),
                post_homing_acceleration: Ok(LegacyValue::Int(100)),
                post_homing_speed_timeout: Ok(LegacyValue::Int(15)),
                scn6_speed: Ok(LegacyValue::Int(10000)),
                scn6_acceleration: Ok(LegacyValue::Int(80)),
                axis_assignment: (0..actuators).map(|i| (i.to_string(), format!("0,{:08X}", i + 1))).collect(),
                refresh_rate: Ok(LegacyValue::Int(300)),
                windows: true,
            });
            let shared = LiveSettings::new(SimTechShared::default());
            Rig {
                driver: SimTechDriver::new(settings.clone(), shared.clone()),
                settings,
                shared,
                host,
                link: LinkState::default(),
                model,
            }
        }

        fn call<R>(&mut self, f: impl FnOnce(&mut SimTechDriver, &mut MotionContext<'_>) -> R) -> R {
            let mut ctx = MotionContext {
                model: motion_model(self.model, true).unwrap(),
                link: &mut self.link,
                host: &mut self.host,
            };
            f(&mut self.driver, &mut ctx)
        }

        fn moves(&mut self, port: &str) -> Vec<Vec<u8>> {
            self.host.log(port).written().into_iter().filter(|w| w.get(2) == Some(&b'a')).collect()
        }
    }

    /// The serial number text: eight upper-case hex digits.
    #[test]
    fn serial_number_text() {
        assert_eq!(convert_sn(0x00AB_12CD), "00AB12CD");
        assert_eq!(convert_sn(0x1_2345_6789), "23456789");
    }

    /// The assignment slot of the matching 'axis,sn' (the first two parts, extra parts ignored), -1 otherwise;
    /// a value with fewer than two parts gives -1 at once, even when a later entry would match; a
    /// matching key that is not an integer is an error.
    #[test]
    fn get_id_lookup() {
        let a = vec![("2".to_string(), "0,AA".to_string()), ("3".to_string(), "1,BB,x".to_string())];
        assert_eq!(assignment_slot(&a, "0", "AA").unwrap(), native::int(2));
        assert_eq!(assignment_slot(&a, "1", "BB").unwrap(), native::int(3));
        assert_eq!(assignment_slot(&a, "0", "BB").unwrap(), native::int(-1));
        let short = vec![("0".to_string(), "x".to_string()), ("1".to_string(), "0,AA".to_string())];
        assert_eq!(assignment_slot(&short, "0", "AA").unwrap(), native::int(-1));
        assert!(assignment_slot(&[("k".into(), "0,AA".into())], "0", "AA").is_err());
    }

    /// Connect: every actuator found on an FTDI 60xx port, powered and homed (homing mode '07'),
    /// then Connected with one port per actuator.
    #[test]
    fn connect_finds_and_homes_every_actuator() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.len(), 4);
        let actuators = rig.shared.get().actuators;
        assert_eq!(actuators[2].sn, "00000003");
        assert_eq!(actuators[2].homing_pattern, Some(7));
        assert_eq!(actuators[2].axis, "0");
        let w = rig.host.log("/dev/ttyUSB0").written();
        assert_eq!(w[0], pack("R4000068000", 0));
        assert!(w.contains(&pack("o0700000000", 0)));
        assert_eq!(
            rig.host.log("/dev/ttyUSB0").opens[0],
            OpenOptions { baud_rate: 115_200, write_timeout: Some(WRITE_TIMEOUT) }
        );
    }

    /// Connect: a model code below 0x3037FE80 or a missing actuator leaves Not connected.
    #[test]
    fn connect_rejects_wrong_model_code() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.host.devices.insert("/dev/ttyUSB3".into(), controller(4, "R400001000"));
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(!rig.link.is_connected());
        assert!(rig.link.ports.is_empty());
    }

    #[test]
    fn simtech_connection_log_velocity_message_uses_port_and_native_status() {
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            let mut rig = Rig::new("STR-SCN6150", 4);
            rig.call(|driver, context| driver.connect(context)).unwrap();
            rig.call(|driver, context| driver.write_velocity(0, native::int(10000), native::int(80), context)).unwrap();
            let mut rejected = Rig::new("STR-SCN6150", 4);
            rejected.host.devices.insert("/dev/ttyUSB3".into(), controller(4, "R400001000"));
            rejected.call(|driver, context| driver.connect(context)).unwrap();
        });
        assert!(messages.iter().any(|message| message == "all_ports connected True"), "{messages:?}");
        assert!(messages.iter().any(|message| message == "all_ports connected False"), "{messages:?}");
        let stages = messages
            .iter()
            .filter(|message| message.starts_with("setting motion status as "))
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(stages, ["setting motion status as Connected", "setting motion status as NOT Connected"]);
        assert!(
            messages
                .iter()
                .any(|message| message == "status of the velocity sent the actuator /dev/ttyUSB0  velocity:   True"),
            "{messages:?}"
        );
    }

    #[test]
    fn native_simtech_diagnostics_connect_identification_and_homing_sequence() {
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            let mut rig = Rig::new("STR-SCN6150", 4);
            rig.call(|driver, context| driver.connect(context)).unwrap();
        });
        assert_eq!(
            &messages[..6],
            [
                "trying to connect",
                "pass checks: switch is on, no motion errors",
                "nbr_controllers that I need to find 4",
                "disconnect from all controllers",
                "create actuator_dict",
                "list serial ports",
            ]
        );
        let identification = [
            "/dev/ttyUSB0 115200 Time to check the protocol",
            "/dev/ttyUSB0 115200 model_code: 34503877712",
            "/dev/ttyUSB0 115200 correct model",
            "/dev/ttyUSB0 115200 sending set_son",
            "/dev/ttyUSB0 115200 seton is done",
            "/dev/ttyUSB0 115200 set iterface",
            "/dev/ttyUSB0 115200 set port no",
            "/dev/ttyUSB0 115200 set default axis",
            "/dev/ttyUSB0 115200 set sn 0",
            "working on the key: 0 port: /dev/ttyUSB0 baud_rate: 115200",
            "/dev/ttyUSB0 115200 read homing pattern",
            "/dev/ttyUSB0 115200 homing pattern: 7",
            "/dev/ttyUSB0 115200 set comport",
            "done with 0",
        ];
        assert!(messages.windows(identification.len()).any(|window| window == identification), "{messages:?}");
        let mut homing = vec!["all_ports connected True".to_owned()];
        for idx in 0..4 {
            homing.push(format!("/dev/ttyUSB{idx} 115200 seting move org to 07"));
        }
        for idx in 0..4 {
            homing.push(format!("/dev/ttyUSB{idx} 115200 homing {idx}"));
            homing.push(format!("/dev/ttyUSB{idx} 115200 homing completed {idx}"));
        }
        homing.extend(["setting motion status as Connected".into(), "Setting the actuator com ports".into()]);
        assert!(messages.ends_with(&homing), "{messages:?}");
    }

    #[test]
    fn native_simtech_diagnostics_velocity_exception_logs_only_error() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|driver, context| driver.connect(context)).unwrap();
        rig.driver.interfaces[0] = None;
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.write_velocity(0, native::int(10000), native::int(80), context)).unwrap();
        });
        assert_eq!(messages, ["setting actuator new velocity /dev/ttyUSB0 10000 80", "error"]);
        let link = crate::motion::driver_support::testing::ScriptedLink::new("/dev/ttyUSB0", |_| reply("R4XX000000"));
        rig.driver.interfaces[0] = Some(TmbsInterface::new(Box::new(link)));
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.write_velocity(0, native::int(10000), native::int(80), context)).unwrap();
        });
        assert_eq!(messages, ["setting actuator new velocity /dev/ttyUSB0 10000 80", "error"]);
    }

    #[test]
    fn native_tmbs_driver_boundary_retains_the_value_error_class() {
        let link = crate::motion::driver_support::testing::ScriptedLink::new("tmbs", |_| reply("R4zz000000"));
        let mut interface = TmbsInterface::new(Box::new(link));
        let error = interface.read_model_code(&mut crate::motion::testing::FakeClock::default()).unwrap_err();
        assert!(matches!(tmbs_error(error).root(), MotionError::Native(detail)
            if detail == "ValueError: invalid literal for int() with base 16: b'zz000000'"));
        let link = crate::motion::driver_support::testing::ScriptedLink::new("tmbs", |_| reply("R48000000000000000"));
        let error = TmbsInterface::new(Box::new(link))
            .read_model_code(&mut crate::motion::testing::FakeClock::default())
            .unwrap_err();
        assert!(matches!(tmbs_error(error), MotionError::Protocol(detail)
            if detail.starts_with("TMBS hexadecimal integer exceeds the current i64 projection: ")));
    }

    /// An error record of the connect: the message, then the traceback of the error being
    /// handled.
    fn record(message: &str, traceback: &str) -> String {
        format!("{message}\n{traceback}")
    }

    #[test]
    fn native_tmbs_power_and_homing_catches_print_the_original_integer_error() {
        use crate::motion::driver_support::testing::{capture_debug_messages, ScriptedLink};
        use crate::motion::testing::FakeClock;
        use std::collections::VecDeque;

        let mut responses = VecDeque::from([reply("R4zz000000"), reply("R40000000B")]);
        let link = ScriptedLink::new("/dev/fake", move |_| responses.pop_front().unwrap());
        let mut interface = TmbsInterface::new(Box::new(link));
        let messages =
            capture_debug_messages(|| SimTechDriver::wait_for_power(&mut interface, 115200, &mut FakeClock::default()));
        assert_eq!(
            messages,
            [
                record(
                    "/dev/fakeerror while powering on invalid literal for int() with base 16: b'zz000000'",
                    "Traceback (most recent call last):\n  File \"backend/controllers/simtechrevolution.py\", line 305, in \
                 backend.controllers.simtechrevolution.connect\n  File \"backend/controllers/pytbmscom.py\", line 571, in \
                 backend.controllers.pytbmscom.TMBSCOMProtocol.readSTATflags\nValueError: invalid literal for int() with base 16: b'zz000000'"
                ),
                "/dev/fake 115200 seton is done".into(),
            ]
        );

        let mut responses = VecDeque::from([reply("R4zz000000"), reply("R40000000B")]);
        let status = pack(&read_register_command(STAT_REGISTER), 0);
        let link = ScriptedLink::new("/dev/fake", move |command| {
            if command == status {
                responses.pop_front().unwrap()
            } else {
                reply("R400000000")
            }
        });
        let mut rig = Rig::new("STR-SCN6150", 0);
        rig.driver.actuators.push(ActuatorRecord {
            com_port: "/dev/fake".into(),
            homing_pattern: Some(7),
            baud_rate: Some(115200),
            ..ActuatorRecord::default()
        });
        rig.driver.interfaces.push(Some(TmbsInterface::new(Box::new(link))));
        let messages = capture_debug_messages(|| rig.call(|driver, context| driver.home_all(context)).unwrap());
        assert_eq!(
            messages,
            [
                "/dev/fake 115200 seting move org to 07".to_string(),
                "/dev/fake 115200 homing 0".into(),
                record(
                    "/dev/fake 115200 error while homing invalid literal for int() with base 16: b'zz000000'",
                    "Traceback (most recent call last):\n  File \"backend/controllers/simtechrevolution.py\", line 368, in \
                 backend.controllers.simtechrevolution.connect\n  File \"backend/controllers/pytbmscom.py\", line 571, in \
                 backend.controllers.pytbmscom.TMBSCOMProtocol.readSTATflags\nValueError: invalid literal for int() with base 16: b'zz000000'"
                ),
                "/dev/fake 115200 homing 0".into(),
                "/dev/fake 115200 homing completed 0".into(),
            ]
        );
    }

    #[test]
    fn native_simtech_diagnostics_scan_and_model_branches() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.connect(context)).unwrap();
        });
        assert!(messages.iter().any(|message| message == "Checking port /dev/ttyUSB0 ttyUSB0 USB VID:PID=0403:6001"));
        assert!(messages.iter().any(|message| message
            == "Trying to instance /dev/ttyUSB0 baud_rate: 115200 ttyUSB0 USB VID:PID=0403:6001 dict_values([])"));
        assert!(messages.iter().any(|message| message == "Trying to instance /dev/ttyUSB0 baud_rate: 115200 ttyUSB0 USB VID:PID=0403:6001 dict_values(['/dev/ttyUSB0'])"));
        assert!(messages.iter().any(|message| message == "/dev/ttyUSB0 115200  is in the allowed ports list"));
        assert!(messages
            .iter()
            .any(|message| message == "/dev/ttyUSB0 115200  is not being used by another controller"));
        assert!(messages.iter().any(|message| message == "/dev/ttyUSB0 115200  The address is valid"));

        for (payload, expected) in [
            ("R400000000", "/dev/ttyUSB0 115200 error while reading model code not simtechrevolution"),
            ("R400001000", "/dev/ttyUSB0 115200 model code is not what I was expecting: 4096"),
        ] {
            let mut rig = Rig::new("STR-SCN6150", 1);
            rig.host.devices.insert("/dev/ttyUSB0".into(), controller(1, payload));
            let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
                rig.call(|driver, context| driver.connect(context)).unwrap();
            });
            assert!(messages.iter().any(|message| message == expected), "{messages:?}");
            assert!(messages
                .ends_with(&["all_ports connected False".into(), "setting motion status as NOT Connected".into()]));
        }
        let mut rig = Rig::new("STR-SCN6150", 1);
        rig.host.ports[0].usb = None;
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.connect(context)).unwrap();
        });
        assert!(messages
            .iter()
            .any(|message| message == "/dev/ttyUSB0 115200  is not being used by another controller"));
        assert!(!messages.iter().any(|message| message.ends_with("The address is valid")));
        rig.host.allowed = "/dev/something-else".into();
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            rig.call(|driver, context| driver.connect(context)).unwrap();
        });
        assert!(messages.iter().any(|message| message == "/dev/ttyUSB0 is NOT in the allowed ports list"));
        assert!(!messages.iter().any(|message| message.starts_with("Checking port")));
    }

    #[test]
    fn native_pnp_simtech_fails_after_allowed_gate_before_open_and_retries() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        let port = rig.host.ports[0].clone();
        let mut details = crate::discovery::ListPortInfo::from_listing(&port);
        details.hwid = None;
        rig.host.listed_details.insert(port.path.clone(), details);
        rig.host.allowed = "/dev/unrelated".into();
        rig.call(|driver, ctx| driver.connect(ctx)).unwrap();
        assert!(rig.host.log(&port.path).opens.is_empty());
        rig.host.allowed.clear();
        let error = rig.call(|driver, ctx| driver.connect(ctx)).unwrap_err();
        assert_eq!(error.to_string(), "TypeError: argument of type 'NoneType' is not iterable");
        assert!(rig.host.log(&port.path).opens.is_empty());
        assert_eq!(rig.shared.get().actuators[0].com_port, "");
        rig.host.listed_details.remove(&port.path);
        rig.call(|driver, ctx| driver.connect(ctx)).unwrap();
        assert!(rig.link.is_connected());
    }

    #[test]
    fn native_simtech_diagnostics_power_and_homing_retry_stages() {
        use crate::motion::driver_support::testing::{capture_debug_messages, ScriptedLink};
        use crate::motion::testing::FakeClock;
        use std::collections::VecDeque;

        let mut responses = VecDeque::from([Vec::new(), reply("R400000000"), reply("R40000000B")]);
        let link = ScriptedLink::new("/dev/fake", move |_| responses.pop_front().expect("three status queries"));
        let mut interface = TmbsInterface::new(Box::new(link));
        let messages =
            capture_debug_messages(|| SimTechDriver::wait_for_power(&mut interface, 115200, &mut FakeClock::default()));
        assert_eq!(
            messages,
            [
                record(
                    "/dev/fakeerror while powering on 'int' object is not subscriptable",
                    "Traceback (most recent call last):\n  File \"backend/controllers/simtechrevolution.py\", line 305, in \
                 backend.controllers.simtechrevolution.connect\nTypeError: 'int' object is not subscriptable"
                ),
                "/dev/fake 115200 turning on".into(),
                "/dev/fake 115200 seton is done".into(),
            ]
        );

        let mut responses = VecDeque::from([reply("R400000000"), Vec::new(), reply("R40000000B")]);
        let status = pack(&read_register_command(STAT_REGISTER), 0);
        let link = ScriptedLink::new("/dev/fake", move |command| {
            if command == status {
                responses.pop_front().expect("three homing queries")
            } else {
                reply("R400000000")
            }
        });
        let mut rig = Rig::new("STR-SCN6150", 0);
        rig.driver.actuators.push(ActuatorRecord {
            com_port: "/dev/fake".into(),
            homing_pattern: Some(7),
            baud_rate: Some(115200),
            ..ActuatorRecord::default()
        });
        rig.driver.interfaces.push(Some(TmbsInterface::new(Box::new(link))));
        let messages = capture_debug_messages(|| rig.call(|driver, context| driver.home_all(context)).unwrap());
        assert_eq!(
            messages,
            [
                "/dev/fake 115200 seting move org to 07".to_string(),
                "/dev/fake 115200 homing 0".into(),
                "/dev/fake 115200 homing 0".into(),
                "/dev/fake 115200 homing 0".into(),
                record(
                    "/dev/fake 115200 error while homing 'int' object is not subscriptable",
                    "Traceback (most recent call last):\n  File \"backend/controllers/simtechrevolution.py\", line 368, in \
                 backend.controllers.simtechrevolution.connect\nTypeError: 'int' object is not subscriptable"
                ),
                "/dev/fake 115200 homing 0".into(),
                "/dev/fake 115200 homing completed 0".into(),
            ]
        );
    }

    #[test]
    fn native_simtech_diagnostics_gate_alarm_and_velocity_updates() {
        use crate::motion::driver_support::testing::capture_debug_messages;

        let mut rig = Rig::new("STR-SCN6150", 4);
        for (switch, error) in [(false, false), (true, true)] {
            rig.settings.update(|settings| {
                settings.motion_switch_on = switch;
                settings.motion_error = error;
            });
            let messages = capture_debug_messages(|| rig.call(|driver, context| driver.connect(context)).unwrap());
            assert_eq!(messages, ["trying to connect"]);
        }
        rig.settings.update(|settings| settings.motion_error = false);
        let alarm = pack(&read_register_command(ALRM_REGISTER), 0);
        rig.host
            .devices
            .get_mut("/dev/ttyUSB0")
            .unwrap()
            .replies
            .iter_mut()
            .find(|(command, _)| *command == alarm)
            .unwrap()
            .1 = reply("OK00000001");
        let messages = capture_debug_messages(|| rig.call(|driver, context| driver.connect(context)).unwrap());
        assert!(messages.iter().any(|message| message == "/dev/ttyUSB0 115200 there is an alarm, trying to reset"));
        let messages = capture_debug_messages(|| {
            rig.call(|driver, context| driver.send_stream(&[0; 6], context)).unwrap();
            rig.call(|driver, context| driver.send_stream(&[0; 6], context)).unwrap();
        });
        assert!(messages.iter().any(|message| message == "setting initial velocity to 10000 80"));
        assert!(messages.iter().any(|message| message == "/dev/ttyUSB0 115200 setting velocity to 800 100"));
        assert!(messages
            .iter()
            .any(|message| message == "status of the velocity sent the actuator /dev/ttyUSB0  velocity:   True"));
    }

    #[test]
    fn native_simtech_inputs_delay_game_errors_until_the_first_frame() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.settings.update(|settings| settings.scn6_speed = Err("ValueError: fixture game speed".into()));
        rig.call(|driver, context| driver.connect(context)).unwrap();
        assert!(rig.link.is_connected());
        assert!(
            matches!(rig.call(|driver, context| driver.send_stream(&[1, 2, 3, 4], context)).unwrap_err().root(), MotionError::Native(detail) if detail == "ValueError: fixture game speed")
        );
        assert_eq!(rig.driver.actuators[0].speed, -1);
        assert_eq!(rig.driver.actuators[0].accel, -1);
    }

    #[test]
    fn native_simtech_inputs_rate_error_keeps_velocity_assignments_before_any_wait() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.settings.update(|settings| settings.refresh_rate = Err("ValueError: fixture refresh rate".into()));
        rig.call(|driver, context| driver.connect(context)).unwrap();
        let before = rig.call(|_, context| context.host.clock().now());
        assert!(
            matches!(rig.call(|driver, context| driver.send_stream(&[1, 2, 3, 4], context)).unwrap_err().root(), MotionError::Native(detail) if detail == "ValueError: fixture refresh rate")
        );
        assert_eq!(rig.driver.actuators[0].speed, 10000);
        assert_eq!(rig.driver.actuators[0].accel, 80);
        assert_eq!(rig.shared.get().actuators[0].speed, 10000);
        assert_eq!(rig.call(|_, context| context.host.clock().now()), before);
    }

    #[test]
    fn simtech_refresh_reads_apply_a_new_rate_after_the_first_write() {
        use crate::motion::driver_support::testing::ScriptedLink;
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|driver, context| driver.connect(context)).unwrap();
        let settings = rig.settings.clone();
        let link = ScriptedLink::new("/dev/ttyUSB0", move |_| {
            settings.update(|settings| settings.refresh_rate = Ok(LegacyValue::Int(30)));
            reply("R400000000")
        });
        let written = link.written.clone();
        rig.driver.interfaces[0] = Some(TmbsInterface::new(Box::new(link)));
        let before = rig.host.now();
        rig.call(|driver, context| driver.write_velocity(0, native::int(10000), native::int(80), context)).unwrap();
        assert_eq!(written.lock().unwrap().len(), VELOCITY_REPEATS);
        assert_eq!(
            rig.host.now() - before,
            openrigcue_model::python_time::sleep_duration(1.0 / 300.0).unwrap()
                + openrigcue_model::python_time::sleep_duration(1.0 / 30.0).unwrap() * 10
        );
    }

    #[test]
    fn simtech_refresh_reads_stop_after_the_completed_write_when_the_next_read_fails() {
        use crate::motion::driver_support::testing::ScriptedLink;
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|driver, context| driver.connect(context)).unwrap();
        let settings = rig.settings.clone();
        let link = ScriptedLink::new("/dev/ttyUSB0", move |_| {
            settings.update(|settings| settings.refresh_rate = Err("ValueError: changed rate fixture".into()));
            reply("R400000000")
        });
        let written = link.written.clone();
        rig.driver.interfaces[0] = Some(TmbsInterface::new(Box::new(link)));
        let before = rig.host.now();
        assert!(
            matches!(rig.call(|driver, context| driver.write_velocity(0, native::int(10000), native::int(80), context)).unwrap_err().root(), MotionError::Native(detail) if detail == "ValueError: changed rate fixture")
        );
        assert_eq!(written.lock().unwrap().len(), 1);
        assert_eq!(rig.host.now() - before, openrigcue_model::python_time::sleep_duration(1.0 / 300.0).unwrap());
        assert_eq!(rig.driver.actuators[0].speed, -1);
        assert_eq!(rig.driver.actuators[0].accel, -1);
    }

    #[test]
    fn simtech_refresh_reads_zero_and_negative_waits_fail_before_any_write_or_delay() {
        for (rate, message) in
            [(0, "ZeroDivisionError: division by zero"), (-1, "ValueError: sleep length must be non-negative")]
        {
            let mut rig = Rig::new("STR-SCN6150", 4);
            rig.settings.update(|settings| settings.refresh_rate = Ok(LegacyValue::Int(rate)));
            rig.call(|driver, context| driver.connect(context)).unwrap();
            let before = rig.host.now();
            let writes = rig.host.log("/dev/ttyUSB0").written().len();
            assert!(
                matches!(rig.call(|driver, context| driver.write_velocity(0, native::int(10000), native::int(80), context)).unwrap_err().root(), MotionError::Native(detail) if detail == message)
            );
            assert_eq!(rig.host.now(), before);
            assert_eq!(rig.host.log("/dev/ttyUSB0").written().len(), writes);
        }
    }

    #[test]
    fn native_driver_errors_keep_rate_error_text_at_the_wait_read() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.settings.update(|settings| {
            settings.refresh_rate = Err("ValueError: invalid literal for int() with base 10: 'bad'".into())
        });
        rig.call(|driver, context| driver.connect(context)).unwrap();
        let before = rig.host.now();
        let error = rig
            .call(|driver, context| driver.write_velocity(0, native::int(10000), native::int(80), context))
            .unwrap_err();
        assert_eq!(error.to_string(), "ValueError: invalid literal for int() with base 10: 'bad'");
        assert_eq!(error.python_str(), "invalid literal for int() with base 10: 'bad'");
        assert_eq!(rig.host.now(), before);
    }

    /// Connect: a port that does not open makes the handling disconnect the current
    /// interface, the previous actuator's: it keeps a closed port, so its homing command is never written.
    #[test]
    fn open_failure_closes_the_previous_actuators_interface() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.host.ports.insert(1, ftdi("/dev/ttyUSB9"));
        rig.host.devices.insert("/dev/ttyUSB9".into(), Device { fail_open: true, ..Device::default() });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected(), "every actuator has a port");
        let homing = pack("o0700000000", 0);
        for closed in ["/dev/ttyUSB0", "/dev/ttyUSB1", "/dev/ttyUSB2"] {
            assert!(!rig.host.log(closed).written().contains(&homing), "{closed}");
        }
        assert!(rig.host.log("/dev/ttyUSB3").written().contains(&homing));
    }

    /// Connect: nothing happens with the motion switch off or a motion error.
    #[test]
    fn connect_gated_by_switch_and_error() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.settings.update(|s| s.motion_error = true);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.host.log("/dev/ttyUSB0").opens.is_empty());
    }

    fn velocity_writes(rig: &mut Rig, port: &str, speed: i64, accel: i64) -> usize {
        rig.host.log(port).written().iter().filter(|f| **f == pack(&velocity_command(speed, accel), 0)).count()
    }

    /// The position pass: a non-zero tune scn6_speed / scn6_acceleration is the first velocity, written 11
    /// times one refresh period apart (no cap on Windows); the next frame rewrites it with the current speed and
    /// acceleration (post-homing values inside the window), then nothing until the window ends and the tune's
    /// values return. Positions are -value for homing pattern 7.
    #[test]
    fn stream_velocity_and_positions() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        let t0 = rig.host.now();
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        assert_eq!(velocity_writes(&mut rig, "/dev/ttyUSB1", 10000, 80), 11);
        assert_eq!(velocity_writes(&mut rig, "/dev/ttyUSB1", 800, 100), 0);
        assert_eq!(rig.moves("/dev/ttyUSB1"), vec![pack("aFFFFFF3800", 0)], "-200");
        let pause = openrigcue_model::python_time::sleep_duration(1.0 / 300.0).unwrap();
        let elapsed = rig.host.now() - t0;
        assert!(elapsed >= pause * 44 && elapsed < Duration::from_secs_f64(44.0 / 30.0), "{elapsed:?}");
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        assert_eq!(velocity_writes(&mut rig, "/dev/ttyUSB1", 800, 100), 11);
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        assert_eq!(velocity_writes(&mut rig, "/dev/ttyUSB1", 800, 100), 11, "no velocity change inside the window");
        assert_eq!(rig.moves("/dev/ttyUSB1").len(), 3);
        rig.host.clock().sleep(Duration::from_secs(16));
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        assert_eq!(velocity_writes(&mut rig, "/dev/ttyUSB1", 10000, 80), 22);
        assert!(rig.shared.get().axis_assignment_status);
    }

    /// The refresh period: `1 / rate`; the 30 Hz cap only off Windows.
    #[test]
    fn refresh_rate_capped_only_off_windows() {
        let rig = Rig::new("STR-SCN6150", 1);
        assert_eq!(
            rig.driver.velocity_pause().unwrap(),
            openrigcue_model::python_time::sleep_duration(1.0 / 300.0).unwrap()
        );
        rig.settings.update(|s| s.windows = false);
        assert_eq!(
            rig.driver.velocity_pause().unwrap(),
            openrigcue_model::python_time::sleep_duration(1.0 / 30.0).unwrap()
        );
        rig.settings.update(|s| s.refresh_rate = Ok(LegacyValue::Int(0)));
        assert!(rig.driver.velocity_pause().is_err());
    }

    #[test]
    fn velocity_wait_uses_native_sleep_rounding() {
        let rig = Rig::new("STR-SCN6150", 1);
        let expected = if cfg!(windows) { 3_333_400 } else { 3_333_334 };
        assert_eq!(rig.driver.velocity_pause().unwrap(), Duration::from_nanos(expected));
    }

    /// The position pass: a tune scn6_speed / scn6_acceleration of 0 makes the first velocity the current
    /// speed and acceleration (the post-homing values inside the window); the next frame sees no change.
    #[test]
    fn zero_game_velocity_uses_get_speed_first() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.settings.update(|s| {
            s.scn6_speed = Ok(LegacyValue::Int(0));
            s.scn6_acceleration = Ok(LegacyValue::Int(0));
        });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        assert_eq!(velocity_writes(&mut rig, "/dev/ttyUSB2", 800, 100), 11);
        assert_eq!(rig.driver.actuators[2].speed, 800);
        assert_eq!(rig.driver.actuators[2].accel, 100);
    }

    /// The position pass: the assignment status becomes true only after every actuator's velocity and slot
    /// lookup passed; a velocity error (scn6_speed no longer an integer after the window) leaves the previous false.
    #[test]
    fn assignment_status_set_after_velocity_loop() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.call(|d, ctx| d.send_stream(&[0; 6], ctx)).unwrap();
        assert!(rig.shared.get().axis_assignment_status);
        let full = rig.settings.get().axis_assignment;
        rig.settings.update(|s| s.axis_assignment.truncate(3));
        rig.call(|d, ctx| d.send_stream(&[0; 6], ctx)).unwrap();
        assert!(!rig.shared.get().axis_assignment_status);
        rig.settings.update(|s| {
            s.axis_assignment = full;
            s.scn6_speed = Err("KeyError: 'scn6_speed'".into());
        });
        rig.host.clock().sleep(Duration::from_secs(16));
        assert!(rig.call(|d, ctx| d.send_stream(&[0; 6], ctx)).is_err());
        assert!(!rig.shared.get().axis_assignment_status);
    }

    /// Connect: a port is claimed as in use right after power-on; when reading its homing pattern then
    /// fails, the error ends the connect (it is not handled) with the fields read so far
    /// in the record and the port still claimed.
    #[test]
    fn failed_probe_keeps_port_claimed() {
        let mut rig = Rig::new("STR-SCN6150", 2);
        let homing = pack(&read_register_command(HOMING_PATTERN_REGISTER), 0);
        let device = rig.host.devices.get_mut("/dev/ttyUSB1").unwrap();
        for (sent, answer) in device.replies.iter_mut() {
            if *sent == homing {
                *answer = reply("R4ZZZZZZZZ");
            }
        }
        let error = rig.call(|d, ctx| d.connect(ctx)).unwrap_err();
        assert_eq!(error.frames().first(), Some(&at("connect", HOMING_PATTERN_LINE)));
        assert_eq!(
            error.exception_line().as_deref(),
            Some("ValueError: invalid literal for int() with base 16: b'ZZZZZZZZ'")
        );
        assert!(!rig.link.is_connected());
        assert_eq!(rig.host.log("/dev/ttyUSB1").opens.len(), 1);
        assert_eq!(rig.driver.ports_in_use[1].as_deref(), Some("/dev/ttyUSB1"));
        let actuators = rig.shared.get().actuators;
        assert_eq!(actuators[0].com_port, "/dev/ttyUSB0");
        assert_eq!(
            (
                actuators[1].axis.as_str(),
                actuators[1].sn.as_str(),
                actuators[1].homing_pattern,
                actuators[1].com_port.as_str()
            ),
            ("0", "00000002", None, "")
        );
    }

    /// Servo on, homing and the velocity write: a controller that never answers them makes each report failure
    /// ('<name> - failed'); the connect still records and homes the actuator and reports Connected, and
    /// a velocity write still makes all 11 writes and stores the values.
    #[test]
    fn silent_actions_return_false_and_carry_on() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        let silent: Vec<Vec<u8>> =
            [pack(SERVO_ON, 0), pack("o0700000000", 0), pack(&velocity_command(10000, 80), 0)].into();
        for device in rig.host.devices.values_mut() {
            device.replies.retain(|(sent, _)| !silent.contains(sent));
        }
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected());
        assert_eq!(rig.shared.get().actuators.len(), 4);
        let w = rig.host.log("/dev/ttyUSB0").written();
        assert!(w.contains(&pack(SERVO_ON, 0)));
        assert!(w.contains(&pack("o0700000000", 0)));
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        rig.call(|d, ctx| d.send_stream(&[100, 200, 300, 400, 0, 0], ctx)).unwrap();
        assert_eq!(
            velocity_writes(&mut rig, "/dev/ttyUSB0", 10000, 80),
            11,
            "values stored after the first 11 unanswered writes"
        );
        assert_eq!(rig.moves("/dev/ttyUSB0").len(), 2);
    }

    /// The position pass: an actuator missing from the axis assignment sets the status false and sends
    /// no position.
    #[test]
    fn incomplete_assignment_stops_the_frame() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.settings.update(|s| s.axis_assignment.truncate(3));
        rig.call(|d, ctx| d.send_stream(&[0; 6], ctx)).unwrap();
        assert!(!rig.shared.get().axis_assignment_status);
        assert!(rig.moves("/dev/ttyUSB0").is_empty());
    }

    /// The traction-loss slot 4 is rescaled 0..15000 → 0..20000 before the homing-pattern formula; other
    /// homing patterns use the maximum minus the value.
    #[test]
    fn traction_loss_slot_and_pattern_formula() {
        let mut rig = Rig::new("STR-SCN6150-TL", 5);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.driver.actuators[0].homing_pattern = Some(1);
        rig.call(|d, ctx| d.send_stream(&[100, 0, 0, 0, 7500, 0], ctx)).unwrap();
        assert_eq!(rig.moves("/dev/ttyUSB0"), vec![pack(&format!("a{:08X}00", 15000 - 100), 0)]);
        assert_eq!(rig.moves("/dev/ttyUSB4"), vec![pack(&format!("a{:08X}00", (-10000i32) as u32), 0)]);
    }

    /// `(function, line)` of the frames recorded for an error, outermost first.
    fn trace(error: &MotionError) -> Vec<(&'static str, u32)> {
        error.frames().iter().map(|frame| (frame.function, frame.line)).collect()
    }

    /// The hardware id test of the connect is not handled: a missing hwid is an error there.
    #[test]
    fn usb_identity_error_carries_the_connect_line() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        let port = rig.host.ports[0].clone();
        let mut details = crate::discovery::ListPortInfo::from_listing(&port);
        details.hwid = None;
        rig.host.listed_details.insert(port.path.clone(), details);
        let error = rig.call(|driver, context| driver.connect(context)).unwrap_err();
        assert_eq!(
            error.python_traceback().unwrap(),
            "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
             backend.controllers.motion_helpers.connect\n  File \"backend/controllers/simtechrevolution.py\", line \
             232, in backend.controllers.simtechrevolution.connect\nTypeError: argument of type 'NoneType' is not \
             iterable\n"
        );
    }

    /// The frames of the frame path (a stream or test frame, then the position pass): the first velocity
    /// reads, the speed and acceleration lookups (also on later frames) with their own lines,
    /// the velocity write pausing for a refresh period, and the slot lookup.
    #[test]
    fn frame_setting_errors_carry_the_function_lines() {
        let missing = |key: &str| Err(format!("KeyError: '{key}'"));
        let failure = |change: &dyn Fn(&mut SimTechSettings), homed_frame: bool| {
            let mut rig = Rig::new("STR-SCN6150", 4);
            rig.call(|driver, context| driver.connect(context)).unwrap();
            if homed_frame {
                rig.call(|driver, context| driver.send_stream(&[0; 6], context)).unwrap();
            }
            rig.settings.update(change);
            rig.call(|driver, context| driver.send_stream(&[0; 6], context)).unwrap_err()
        };
        let sap = "send_actuator_pos";
        let error = failure(&|s| s.scn6_speed = missing("scn6_speed"), false);
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 466)]);
        assert_eq!(error.exception_line().as_deref(), Some("KeyError: 'scn6_speed'"));
        let error = failure(&|s| s.scn6_acceleration = missing("scn6_acceleration"), false);
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 479)]);
        // A tune speed of 0 asks for the current speed: the timeout, then the post-homing value.
        let error = failure(
            &|s| {
                s.scn6_speed = Ok(LegacyValue::Int(0));
                s.post_homing_speed_timeout = missing("post_homing_speed_timeout");
            },
            false,
        );
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 469), ("get_speed", 413)]);
        let error = failure(
            &|s| {
                s.scn6_speed = Ok(LegacyValue::Int(0));
                s.post_homing_speed = missing("post_homing_speed");
            },
            false,
        );
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 469), ("get_speed", 416)]);
        let error = failure(
            &|s| {
                s.scn6_acceleration = Ok(LegacyValue::Int(0));
                s.post_homing_acceleration = missing("post_homing_acceleration");
            },
            false,
        );
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 482), ("get_accel", 432)]);
        // The first velocity write pauses for a refresh period.
        let error = failure(&|s| s.refresh_rate = Ok(LegacyValue::Int(0)), false);
        assert_eq!(
            trace(&error),
            [("send_stream", 539), (sap, 492), ("write_velocity", 142), ("get_refresh_rate", 125)]
        );
        assert_eq!(error.exception_line().as_deref(), Some("ZeroDivisionError: division by zero"));
        let error = failure(&|s| s.refresh_rate = Ok(LegacyValue::Int(-5)), false);
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 492), ("write_velocity", 142)]);
        assert_eq!(error.exception_line().as_deref(), Some("ValueError: sleep length must be non-negative"));
        let error = failure(&|s| s.refresh_rate = missing("refresh_rate"), false);
        assert_eq!(
            trace(&error),
            [("send_stream", 539), (sap, 492), ("write_velocity", 142), ("get_refresh_rate", 120)]
        );
        // Later frames compare with the current speed and acceleration.
        let error = failure(&|s| s.post_homing_speed_timeout = missing("post_homing_speed_timeout"), true);
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 494), ("get_speed", 413)]);
        let error = failure(&|s| s.post_homing_acceleration = missing("post_homing_acceleration"), true);
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 495), ("get_accel", 432)]);
        // The slot lookup: the key as an integer.
        let error = failure(&|s| s.axis_assignment[0].0 = "one".into(), true);
        assert_eq!(trace(&error), [("send_stream", 539), (sap, 502), ("get_id", 403)]);
        assert_eq!(
            error.exception_line().as_deref(),
            Some("ValueError: invalid literal for int() with base 10: 'one'")
        );

        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|driver, context| driver.connect(context)).unwrap();
        rig.settings.update(|s| s.scn6_speed = missing("scn6_speed"));
        let error = rig.call(|driver, context| driver.send_test(&[0; 6], context)).unwrap_err();
        assert_eq!(trace(&error), [("send_test", 543), (sap, 466)]);
    }

    /// The homing movement of the connect is not handled: a reply that is not hexadecimal
    /// fails inside the protocol (the homing operation, then the reply parsing).
    #[test]
    fn homing_reply_error_carries_the_connect_and_protocol_frames() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        let mut device = controller(1, "R4808976450");
        device.replies.retain(|(command, _)| command != &pack("o0700000000", 0));
        device.replies.push((pack("o0700000000", 0), reply("R4zz000000")));
        rig.host.devices.insert("/dev/ttyUSB0".into(), device);
        let error = rig.call(|driver, context| driver.connect(context)).unwrap_err();
        assert_eq!(
            trace(&error),
            [("connect", 361), ("TMBSCOMProtocol.move_org", 741), ("TMBSCOMProtocol.parseADirectResponseFormat", 217)]
        );
        assert_eq!(
            error.exception_line().as_deref(),
            Some("ValueError: invalid literal for int() with base 16: b'zz'")
        );
    }

    /// A model-code read that fails is logged with the port ("error while reading
    /// model code not simtechrevolution"), the interface is disconnected and the next attempt follows; the
    /// "Error while instancing" handling is not involved (it only covers the opening of the port).
    #[test]
    fn model_code_read_exception_is_the_model_code_handlers() {
        use crate::motion::driver_support::testing::capture_debug_messages;
        let mut rig = Rig::new("STR-SCN6150", 1);
        rig.host.devices.insert("/dev/ttyUSB0".into(), controller(1, "R4zz000000"));
        let messages = capture_debug_messages(|| {
            rig.call(|driver, context| driver.connect(context)).unwrap();
        });
        assert!(!messages.iter().any(|m| m.contains("Error while instancing")), "{messages:?}");
        let handled: Vec<&String> =
            messages.iter().filter(|m| m.contains("error while reading model code not simtechrevolution")).collect();
        assert!(!handled.is_empty());
        let expected = record(
            "/dev/ttyUSB0error while reading model code not simtechrevolution",
            "Traceback (most recent call last):\n  File \"backend/controllers/simtechrevolution.py\", line 257, in \
                 backend.controllers.simtechrevolution.connect\n  File \"backend/controllers/pytbmscom.py\", line 561, in \
                 backend.controllers.pytbmscom.TMBSCOMProtocol.readModelCode\nValueError: invalid literal for int() with base 16: b'zz000000'",
        );
        assert!(handled.iter().all(|m| **m == expected), "{handled:?}");
        assert!(!rig.link.is_connected());
        assert!(rig.driver.interfaces[0].is_none(), "the rejected interface is disconnected");
    }

    /// A port that does not open: the error log ("Error while instancing
    /// the serial port, will disconnect") writes no record; the logging error report goes to standard error,
    /// chained to the open failure, with the message and its unused argument.
    #[test]
    fn instancing_failure_reports_the_logging_argument_error() {
        use crate::motion::driver_support::testing::{capture_diagnostics, Diagnostic};
        let mut rig = Rig::new("STR-SCN6150", 1);
        rig.host.devices.insert("/dev/ttyUSB0".into(), Device { fail_open: true, ..Device::default() });
        let diagnostics = capture_diagnostics(|| rig.call(|driver, context| driver.connect(context)).unwrap());
        assert!(!diagnostics.iter().any(|d| matches!(d, Diagnostic::Log(m) if m.contains("Error while instancing"))));
        let reports: Vec<&String> = diagnostics
            .iter()
            .filter_map(|d| match d {
                Diagnostic::Stderr(text) => Some(text),
                _ => None,
            })
            .collect();
        assert!(!reports.is_empty());
        // No interface was ever opened: the disconnect fails while the open
        // failure is being handled, and is logged with both tracebacks.
        let opened =
            "Traceback (most recent call last):\n  File \"backend/controllers/simtechrevolution.py\", line 240, \
                      in backend.controllers.simtechrevolution.connect\n  File \"backend/controllers/pytbmscom.py\", \
                      line 18, in backend.controllers.pytbmscom.TMBSCOMProtocol.__init__\n  File \
                      \"serial\\serialwin32.py\", line 33, in __init__\n  File \"serial\\serialutil.py\", line 244, in \
                      __init__\n  File \"serial\\serialwin32.py\", line 64, in open\n\
                      serial.serialutil.SerialException: busy";
        let disconnect = record(
            "/dev/ttyUSB0error while disconnecting",
            &format!(
                "{opened}\n\nDuring handling of the above exception, another exception occurred:\n\n\
                 Traceback (most recent call last):\n  File \"backend/controllers/simtechrevolution.py\", line 246, \
                 in backend.controllers.simtechrevolution.connect\n\
                 UnboundLocalError: local variable 'interface' referenced before assignment"
            ),
        );
        let disconnects: Vec<&Diagnostic> = diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::Log(m) if m.contains("error while disconnecting")))
            .collect();
        assert_eq!(disconnects, vec![&Diagnostic::Log(disconnect); reports.len()]);
        for report in reports {
            assert!(
                report.starts_with(
                    "--- Logging error ---\nTraceback (most recent call last):\n  File \
                     \"backend/controllers/simtechrevolution.py\", line 240, in \
                     backend.controllers.simtechrevolution.connect\n  File \"backend/controllers/pytbmscom.py\", \
                     line 18, in backend.controllers.pytbmscom.TMBSCOMProtocol.__init__\n"
                ),
                "{report}"
            );
            assert!(report.contains("serial.serialutil.SerialException: busy\n\nDuring handling"), "{report}");
            assert!(
                report.ends_with(
                    "Message: '/dev/ttyUSB0Error while instancing the serial port, will disconnect'\n\
                     Arguments: ('busy',)\n"
                ),
                "{report}"
            );
        }
    }

    /// The steps after the model code check are not handled: a reply that is not hexadecimal ends
    /// the connect with the frame of its step. Before `set iterface` the record has no interface; after it
    /// the record keeps the interface, its port open, and the claim.
    #[test]
    fn later_read_exceptions_leave_connect_with_their_statement() {
        let register = |addr: &str| pack(&read_register_command(addr), 0);
        for (addr, line, holds_interface) in [(ALRM_REGISTER, 293, false), (SERIAL_NUMBER_REGISTER, 331, true)] {
            let mut rig = Rig::new("STR-SCN6150", 2);
            let mut device = controller(1, "R4808976450");
            device.replies.retain(|(command, _)| *command != register(addr));
            device.replies.push((register(addr), reply("R4zz000000")));
            rig.host.devices.insert("/dev/ttyUSB0".into(), device);
            let error = rig.call(|driver, context| driver.connect(context)).unwrap_err();
            assert_eq!(error.frames().first(), Some(&at("connect", line)), "{error:?}");
            assert!(error.exception_line().is_some_and(|l| l.starts_with("ValueError: ")), "{error:?}");
            assert!(rig.host.log("/dev/ttyUSB1").opens.is_empty(), "the scan ends there");
            assert_eq!(rig.driver.interfaces[0].is_some(), holds_interface);
            assert_eq!(rig.driver.ports_in_use[0].is_some(), holds_interface);
            assert!(!rig.link.is_connected());
        }
    }

    /// The slot read with a hand-edited assignment id of 12 or more: an index error there, on its way out
    /// through the stream frame.
    #[test]
    fn assignment_id_past_the_list_raises_at_the_subscript() {
        let mut rig = Rig::new("STR-SCN6150", 4);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.settings.update(|s| s.axis_assignment[0].0 = "12".into());
        let error = rig.call(|d, ctx| d.send_stream(&[0; 12], ctx)).unwrap_err();
        assert_eq!(error.frames(), [at("send_stream", 539), at(SEND_ACTUATOR_POS, MOTOR_SUBSCRIPT_LINE)]);
        assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        assert_eq!(MOTOR_SUBSCRIPT_LINE, 520);
    }

    /// The hexadecimal fields of the velocity command at any width, and the refresh period of a wide rate (a
    /// float that can be slept).
    #[test]
    fn native_velocity_values_keep_their_width_until_the_command_text() {
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 68);
        assert_eq!(native::percent_hex(&wide, 4).unwrap(), "100000000000000000");
        assert_eq!(native::percent_hex(&native::int(-5), 4).unwrap(), "-005");
        assert_eq!(native::percent_hex(&native::int(5000), 4).unwrap(), "1388");
        assert_eq!(
            native::percent_hex(&LegacyValue::Float(1.0), 4).unwrap_err().exception_line().unwrap(),
            "TypeError: %x format: an integer is required, not float"
        );
        let command = crate::motion::tmbs::Command::VelocityHex { speed: "1388".into(), accel: "0050".into() };
        assert_eq!(command.text(), crate::motion::tmbs::Command::Velocity { speed: 5000, accel: 80 }.text());
        let rig = Rig::new("STR-SCN6150", 4);
        rig.settings.update(|settings| settings.refresh_rate = Ok(wide));
        // 1 / 2**68 s is rounded away from zero to whole nanoseconds.
        assert_eq!(rig.driver.velocity_pause().unwrap(), openrigcue_model::python_time::sleep_duration(1e-9).unwrap());
    }

    /// The refresh rate is read as an integer on each use: a live 0 is a
    /// division by zero although the refreshed value is 30.
    #[test]
    fn native_refresh_rate_is_read_live_per_velocity_write() {
        use crate::motion::driver_support::testing::FakeConfig;
        let rig = Rig::new("STR-SCN6150", 4);
        rig.settings.update(|settings| settings.refresh_rate = Ok(native::int(30)));
        let config = std::sync::Arc::new(FakeConfig::default());
        rig.settings.read(|s| s.runtime.install_config(config.clone()));
        config.set("MOTIONBOX", "refresh_rate", native::int(0));
        let error = rig.driver.velocity_pause().unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "ZeroDivisionError: division by zero");
        config.set("MOTIONBOX", "refresh_rate", native::int(10));
        assert_eq!(rig.driver.velocity_pause().unwrap(), openrigcue_model::python_time::sleep_duration(0.1).unwrap());
    }
}

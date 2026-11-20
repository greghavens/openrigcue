//! PT-Actuator CANBus models behind a serial CAN bridge: the connect with homing, the position
//! frames of stream and test, the wait mode after a connect, and the homing and position
//! frame layouts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use crate::motion::driver_support::CornerTravel;
use crate::motion::native::{self, NativeSetting};
use crate::motion::pt_actuator::MotionWarning;
use crate::motion::thanos::{ActuatorLayout, VerticalPositions};
use crate::motion::{
    python_print, ActuatorPositions, DriverFactory, LinkStatus, LiveSettings, MotionContext, MotionDriver, MotionError,
    PyFrame,
};
use crate::serial::{Link, OpenOptions};
use num_traits::ToPrimitive;
use openrigcue_model::native_value::LegacyValue;

/// The port-not-open error text of a write on a closed port.
const PORT_NOT_OPEN: &str = "Attempting to use a port that is not open";

/// CAN ids addressed on connect and in every frame: 1 to 7.
pub const CAN_IDS: std::ops::Range<u8> = 1..8;
/// Speeds outside `0..=8192` are replaced by a default.
const SPEED_MAX: i64 = 8192;
const DEFAULT_PARKING_SPEED: i64 = 500;
const DEFAULT_MAP_MIN: i64 = 1000;
const DEFAULT_MAP_MAX: i64 = 8192;
/// Speed used while the vehicle is not moving (telemetry speed not above 0).
const STANDSTILL_SPEED: i64 = 1000;
/// `reaction_speed` (the "Action Mode" slider) spans 70..100.
const REACTION_MIN: f64 = 70.0;
const REACTION_MAX: f64 = 100.0;
/// How long the wait mode lasts after it is set.
const WAIT_MODE_DURATION: Duration = Duration::from_secs(5);
/// Idle posture of a test frame with no game running: verticals at 5 %, the rest centred.
pub const IDLE_POSTURE: [i64; 12] = [3276, 3276, 3276, 3276, 32768, 32768, 32768, 32768, 32768, 32768, 32768, 32768];
/// Reverse-flag pivot (`65536 - value`) and slot indices of `reverse_controller_port_5..7`.
const REVERSE_PIVOT: i64 = 65_536;
const REVERSIBLE_SLOTS: [usize; 3] = [4, 5, 6];

/// `[high, low]` of the value modulo 65536.
fn int_to_bytes(value: i64) -> [u8; 2] {
    (value.rem_euclid(65_536) as u16).to_be_bytes()
}

/// Homing frame for a CAN id and a speed; the speed's high byte is sent twice.
pub fn findz_frame(can_id: u8, speed: i64) -> [u8; 12] {
    let s = int_to_bytes(speed);
    [0x00, can_id, 0x00, 0x1A, 0x07, s[0], s[0], 0x53, 0x00, 0x00, 0x0D, 0x0A]
}

/// [`findz_frame`] of a speed already reduced to 16 bits.
pub fn findz_frame_word(can_id: u8, speed: u16) -> [u8; 12] {
    findz_frame(can_id, i64::from(speed))
}

/// Position frame for a CAN id, a position and a speed; the speed's high byte is sent
/// twice and the low byte never.
pub fn position_frame(can_id: u8, position: i64, speed: i64) -> [u8; 12] {
    let s = int_to_bytes(speed);
    let p = int_to_bytes(position);
    [0x00, can_id, 0x00, 0x1A, 0x1D, s[0], s[0], 0x05, p[0], p[1], 0x0D, 0x0A]
}

/// [`position_frame`] of a position and speed already reduced to 16 bits.
pub fn position_frame_words(can_id: u8, position: u16, speed: u16) -> [u8; 12] {
    position_frame(can_id, i64::from(position), i64::from(speed))
}

/// The bytes as upper-case two-digit hex joined by spaces (debug log only).
fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")
}

/// `value` when it is within `0..=8192`, else `default`; compared at any width.
fn speed_or(value: LegacyValue, default: i64) -> Result<LegacyValue, MotionError> {
    let inside = native::le(&native::int(0), &value)? && native::le(&value, &native::int(SPEED_MAX))?;
    Ok(if inside { value } else { native::int(default) })
}

/// The file name in the frames of this driver's errors.
const PTCAN_FILE: &str = "backend/controllers/ptcan.py";

/// What the driver reads. The configuration values are the raw values of their keys (or the error of the
/// lookup); each use converts it to an integer itself.
#[derive(Debug, Clone, PartialEq)]
pub struct PtCanSettings {
    /// `[PT-ACTUATOR] pt_com_port`; empty disables connecting.
    pub pt_com_port: String,
    /// `[PT-ACTUATOR] pt_com_baud_rate`.
    pub baud_rate: NativeSetting,
    /// `[PT-ACTUATOR] parking_speed`.
    pub parking_speed: NativeSetting,
    /// `[PT-ACTUATOR] reaction_speed_map_min`.
    pub reaction_speed_map_min: NativeSetting,
    /// `[PT-ACTUATOR] reaction_speed_map_max`.
    pub reaction_speed_map_max: NativeSetting,
    /// `[PT-ACTUATOR] park_to_motion_speed_delay_s`.
    pub park_to_motion_speed_delay_s: NativeSetting,
    /// `[PT-ACTUATOR] reverse_controller_port_5/6/7` as booleans.
    pub reverse_ports: [bool; 3],
    /// `[MOTIONBOX] reaction_speed`.
    pub reaction_speed: NativeSetting,
    /// `[MOTION_COMPENSATION] vertical_actuators_stroke`.
    pub compensation_stroke_mm: NativeSetting,
    /// The telemetry speed (0 when unavailable).
    pub vehicle_speed: f64,
    /// The motion test mode: a test frame acts on 1 and 0 only.
    pub motion_test_mode: i64,
    /// Whether no game is running.
    pub no_game_running: bool,
    /// Live state and telemetry for the steps that read them.
    pub runtime: crate::motion::driver_support::RuntimeReader,
}

impl PtCanSettings {
    /// The settings with the state and telemetry as they are now.
    pub fn live(&self) -> PtCanSettings {
        let mut live = self.clone();
        if let Some(view) = self.runtime.read() {
            live.vehicle_speed = view.speed;
            live.motion_test_mode = i64::from(view.motion_test_mode);
            live.no_game_running = view.game_running == "none";
        }
        live
    }
}

/// Every configuration key missing (its lookup is a key error).
impl Default for PtCanSettings {
    fn default() -> Self {
        let missing = |key: &str| Err(format!("KeyError: '{key}'"));
        PtCanSettings {
            pt_com_port: String::new(),
            baud_rate: missing("pt_com_baud_rate"),
            parking_speed: missing("parking_speed"),
            reaction_speed_map_min: missing("reaction_speed_map_min"),
            reaction_speed_map_max: missing("reaction_speed_map_max"),
            park_to_motion_speed_delay_s: missing("park_to_motion_speed_delay_s"),
            reverse_ports: [false; 3],
            reaction_speed: missing("reaction_speed"),
            compensation_stroke_mm: missing("vertical_actuators_stroke"),
            vehicle_speed: 0.0,
            motion_test_mode: 0,
            no_game_running: true,
            runtime: Default::default(),
        }
    }
}

/// A raw setting as an integer, at any width.
fn int_of(setting: &NativeSetting) -> Result<LegacyValue, MotionError> {
    native::to_int(&setting.clone().map_err(MotionError::Native)?)
}

/// A timer: runs `function` on a thread of its own after `interval`.
pub type TimerSpawner = Box<dyn FnMut(Duration, Box<dyn FnOnce() + Send>) + Send>;

/// The real timer: a detached thread that sleeps on the wall clock.
pub fn system_timer() -> TimerSpawner {
    Box::new(|interval, function| {
        let started = std::thread::Builder::new().name("Timer".into()).spawn(move || {
            std::thread::sleep(interval);
            function();
        });
        if let Err(e) = started {
            tracing::error!("cannot start the wait_mode timer: {e}");
        }
    })
}

/// The wait mode (on at start).
#[derive(Debug, Clone)]
struct WaitMode(Arc<AtomicBool>);

impl Default for WaitMode {
    fn default() -> Self {
        WaitMode(Arc::new(AtomicBool::new(true)))
    }
}

impl WaitMode {
    fn active(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Sets the wait mode and prints the new value.
    fn store(&self, value: bool) {
        self.0.store(value, Ordering::SeqCst);
        python_print(&[&format!("wait_mode set to {}", if value { "True" } else { "False" })]);
    }
}

/// The PT CANBus driver.
pub struct PtCanDriver {
    settings: LiveSettings<PtCanSettings>,
    warning: MotionWarning,
    /// The open serial port.
    port: Option<Box<dyn Link>>,
    wait_mode: WaitMode,
    /// Starts the timers that clear the wait mode.
    timer: TimerSpawner,
    vertical_positions: Option<VerticalPositions>,
    /// The list of the last frame, after its reverse switches (a reversed slot becomes 65536 minus itself, in place).
    edited: Option<Vec<i64>>,
    /// Whether the last `&[i64]` frame had a slot written.
    wrote: bool,
}

impl PtCanDriver {
    pub fn new(settings: LiveSettings<PtCanSettings>, warning: MotionWarning) -> Self {
        PtCanDriver {
            settings,
            warning,
            port: None,
            wait_mode: WaitMode::default(),
            timer: system_timer(),
            vertical_positions: None,
            edited: None,
            wrote: false,
        }
    }

    pub fn factory(settings: LiveSettings<PtCanSettings>, warning: MotionWarning) -> DriverFactory {
        Box::new(move || Box::new(PtCanDriver::new(settings.clone(), warning.clone())))
    }

    /// Last actuator positions (mm).
    pub fn vertical_positions(&self) -> Option<VerticalPositions> {
        self.vertical_positions
    }

    /// The same driver starting its wait-mode timers through `timer` (tests run them by hand).
    pub fn with_timer(mut self, timer: TimerSpawner) -> Self {
        self.timer = timer;
        self
    }

    /// Whether the wait mode is on.
    pub fn wait_mode(&self) -> bool {
        self.wait_mode.active()
    }

    /// Sets the wait mode (printed), then starts a 5 s timer that
    /// clears it (printed). Timers of earlier calls are not cancelled.
    fn set_wait_mode(&mut self) {
        self.wait_mode.store(true);
        let wait_mode = self.wait_mode.clone();
        (self.timer)(WAIT_MODE_DURATION, Box::new(move || wait_mode.store(false)));
    }

    /// The speed of every position frame: in wait mode the parking speed as an integer, else
    /// both map bounds as integers and, while the vehicle moves, the reaction speed rescaled from 70..100 to
    /// those bounds and truncated; each value outside `0..=8192` replaced by its default.
    fn frame_speed(&self, settings: &PtCanSettings) -> Result<LegacyValue, MotionError> {
        if self.wait_mode.active() {
            return speed_or(
                int_of(&settings.runtime.value("PT-ACTUATOR", "parking_speed", &settings.parking_speed))?,
                DEFAULT_PARKING_SPEED,
            );
        }
        let map_min = speed_or(
            int_of(&settings.runtime.value("PT-ACTUATOR", "reaction_speed_map_min", &settings.reaction_speed_map_min))?,
            DEFAULT_MAP_MIN,
        )?;
        let map_max = speed_or(
            int_of(&settings.runtime.value("PT-ACTUATOR", "reaction_speed_map_max", &settings.reaction_speed_map_max))?,
            DEFAULT_MAP_MAX,
        )?;
        if settings.live().vehicle_speed > 0.0 {
            // The speed is the reaction speed rescaled from 70..100 to the bounds, truncated.
            let line175 = PyFrame::new(PTCAN_FILE, "send_pt", 175);
            let reaction = int_of(&settings.runtime.value("MOTIONBOX", "reaction_speed", &settings.reaction_speed))
                .map_err(|e| e.in_frame(line175))?;
            let mapped = native::map([
                &reaction,
                &native::float(REACTION_MIN),
                &native::float(REACTION_MAX),
                &map_min,
                &map_max,
            ])
            .map_err(|e| e.in_frame(line175))?;
            native::trunc_float(mapped)
        } else {
            Ok(native::int(STANDSTILL_SPEED))
        }
    }

    /// Sends one position frame per CAN id from the caller's list.
    fn send_pt(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let settings = self.settings.get();
        for (port, (&slot, &typed)) in REVERSIBLE_SLOTS.iter().zip(&settings.reverse_ports).enumerate() {
            // The reverse switch of the port as a boolean, read now.
            let reverse = settings
                .runtime
                .switch("PT-ACTUATOR", &format!("reverse_controller_port_{}", port + 5), &Ok(typed))
                .map_err(MotionError::Native)?;
            if reverse {
                // The slot becomes 65536 minus itself, written into the caller's list.
                let value = native::item(motors, slot)?;
                let value = native::sub(&native::int(REVERSE_PIVOT), &value)?;
                native::assign(motors, slot, value)?;
                self.wrote = true;
            }
        }
        if !ctx.link.is_connected() || self.port.is_none() {
            return Ok(());
        }
        // The actuator positions: four slots rescaled from 0..65535 to 0..the stroke setting as an integer.
        let layout = ActuatorLayout::from_model_name(ctx.model.name);
        // One entry per line: (2 Front + 1 Rear), (1 Front + 2 Rear), else.
        let first_line = match layout {
            ActuatorLayout::TwoFrontOneRear => 134,
            ActuatorLayout::OneFrontTwoRear => 140,
            ActuatorLayout::FourCorners => 146,
        };
        let mut corners = [0.0; 4];
        for ((corner, slot), line) in corners.iter_mut().zip(layout.slots()).zip(first_line..) {
            let entry = PyFrame::new(PTCAN_FILE, "send_pt", line);
            let value = native::item(motors, slot).map_err(|e| e.in_frame(entry))?;
            let stroke = int_of(&settings.runtime.value(
                "MOTION_COMPENSATION",
                "vertical_actuators_stroke",
                &settings.compensation_stroke_mm,
            ))
            .map_err(|e| e.in_frame(entry))?;
            *corner = native::map([&value, &native::int(0), &native::int(65_535), &native::int(0), &stroke])
                .map_err(|e| e.in_frame(entry))?;
        }
        let [front_left, front_right, rear_left, rear_right] = corners;
        self.vertical_positions = Some(VerticalPositions { front_left, front_right, rear_left, rear_right });
        // A failed write is handled inside the CAN-id loop:
        // the status becomes 'Not Connected', the motion port entry 0 is removed and the port closed, and the
        // loop goes on. The next write on the closed port fails with the port-not-open error and is handled the
        // same way; removing entry 0 a second time is a key error that ends the pass.
        let mut closed = false;
        for can_id in CAN_IDS {
            let speed = self.frame_speed(&settings)?;
            let position = native::item(motors, usize::from(can_id) - 1)?;
            // The position frame of the CAN id takes slot `can_id - 1`; position and speed are reduced to 16 bits.
            let frame = position_frame_words(can_id, native::c_uint16(&position)?, native::c_uint16(&speed)?);
            // The write; its error is handled here and its text logged.
            let written = match self.port.as_mut() {
                Some(port) if !closed => port.write_all(&frame),
                _ => Err(crate::serial::LinkError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    PORT_NOT_OPEN,
                ))),
            };
            if let Err(e) = written {
                debug!("error writing to controller {}", crate::motion::python::link_error_str(&e));
                ctx.link.status = LinkStatus::NotConnectedAfterWriteError;
                if ctx.link.ports.remove(&0).is_none() {
                    // Removing motion port entry 0 fails while the write error is being handled
                    // (the write error's own traceback precedes this one in the log).
                    let key_error =
                        MotionError::Native("KeyError: 0".into()).in_frame(PyFrame::new(PTCAN_FILE, "send_pt", 187));
                    let handled = MotionError::Link(e)
                        .in_serial_call(crate::motion::SerialCall::Write, PyFrame::new(PTCAN_FILE, "send_pt", 183));
                    // The handled write error is printed first where its frames are known.
                    return Err(if handled.frames().is_empty() {
                        key_error
                    } else {
                        key_error.while_handling(handled)
                    });
                }
                // The port is closed but kept; closing cannot fail here.
                self.port = None;
                closed = true;
            }
        }
        Ok(())
    }

    /// An `&[i64]` frame through the dynamic-value path; the reversed slots become the edited frame.
    fn send_pt_compat(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.wrote = false;
        let (result, list) = native::compat(motors, |list| self.send_pt(list, ctx));
        self.edited = self.wrote.then(|| native::edited_vec(&list, motors));
        result
    }
}

impl MotionDriver for PtCanDriver {
    /// Connects: open the configured port, send find-z (homing) to CAN ids 1..=7 at the parking
    /// speed, wait `park_to_motion_speed_delay_s`, then `Connected`, the motion warning cleared and 5 s of
    /// wait mode.
    ///
    /// A port that cannot be opened, or a baud rate that is not an integer, ends the connect without
    /// an error. Motion port entry 0 is assigned before the port is opened. The later steps are not guarded;
    /// when one fails, the driver keeps the port it opened.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let settings = self.settings.get();
        // Each access reads `PT-ACTUATOR.pt_com_port` as it is now.
        let raw_port = || {
            settings.runtime.value("PT-ACTUATOR", "pt_com_port", &Ok(LegacyValue::Str(settings.pt_com_port.clone())))
        };
        let port_text = || -> Result<String, MotionError> {
            let raw = raw_port().map_err(MotionError::Native)?;
            Ok(raw.python_string().and_then(|text| text.scalar_string()).unwrap_or_else(|| native::str_of(&raw)))
        };
        if native::eq(&raw_port().map_err(MotionError::Native)?, &LegacyValue::from(""))? {
            return Ok(());
        }
        ctx.link.ports.insert(0, port_text()?);
        // A baud rate that is not an integer, or not a valid rate, ends the connect quietly.
        let baud = int_of(&settings.runtime.value("PT-ACTUATOR", "pt_com_baud_rate", &settings.baud_rate))
            .ok()
            .and_then(|baud| baud.integer())
            .and_then(|baud| u32::try_from(baud).ok());
        let Some(baud_rate) = baud else {
            return Ok(());
        };
        // The port held until now is released only once the new one exists.
        // A failure to read the port setting here also ends the connect quietly.
        let Ok(path) = port_text() else {
            return Ok(());
        };
        let Ok(port) = ctx.host.open_serial(&path, &OpenOptions { baud_rate, write_timeout: None }) else {
            return Ok(());
        };
        let port = self.port.insert(port);
        for can_id in CAN_IDS {
            let speed = speed_or(
                int_of(&settings.runtime.value("PT-ACTUATOR", "parking_speed", &settings.parking_speed))?,
                DEFAULT_PARKING_SPEED,
            )?;
            let frame = findz_frame_word(can_id, native::c_uint16(&speed)?);
            // Logged, then written; a failed write is logged as an error.
            debug!("{can_id} find z: {}", hex_string(&frame));
            if let Err(e) = port.write_all(&frame) {
                tracing::error!("Failed to findz {}", crate::motion::python::link_error_str(&e));
            }
        }
        // The pause is the delay setting as an integer of seconds: one whose nanoseconds do not
        // fit 64 bits is an overflow error, a negative one a value error; either aborts the connect before
        // 'Connected'.
        let delay = native::to_bigint(&int_of(&settings.runtime.value(
            "PT-ACTUATOR",
            "park_to_motion_speed_delay_s",
            &settings.park_to_motion_speed_delay_s,
        ))?)?;
        let nanos = (&delay * 1_000_000_000i64)
            .to_i64()
            .ok_or_else(|| native::raised("OverflowError: timestamp too large to convert to C PyTime_t"))?;
        let nanos =
            u64::try_from(nanos).map_err(|_| native::raised("ValueError: sleep length must be non-negative"))?;
        ctx.host.clock().sleep(Duration::from_nanos(nanos));
        ctx.link.status = LinkStatus::Connected;
        self.warning.set(String::new());
        self.set_wait_mode();
        Ok(())
    }

    /// Disconnecting has no observable action.
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        Ok(())
    }

    /// A stream frame: the position frames while `Connected`.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if ctx.link.is_connected() {
            // The position frames are sent; an error passes this frame.
            self.send_pt_compat(motors, ctx).map_err(|e| e.through(PyFrame::new(PTCAN_FILE, "send_stream", 205)))?;
        }
        Ok(())
    }

    /// A stream frame on the caller's list.
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if ctx.link.is_connected() {
            self.send_pt(motors, ctx).map_err(|e| e.through(PyFrame::new(PTCAN_FILE, "send_stream", 205)))?;
        }
        Ok(())
    }

    /// A test frame: with `motion_test_mode` 1 the given frame; with 0 the idle posture when no game
    /// runs, else the given frame; any other value sends nothing.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if !ctx.link.is_connected() {
            return Ok(());
        }
        let settings = self.settings.get().live();
        let call = |line: u32| move |e: MotionError| e.through(PyFrame::new(PTCAN_FILE, "send_test", line));
        match settings.motion_test_mode {
            1 => self.send_pt_compat(motors, ctx).map_err(call(212)),
            0 if settings.no_game_running => {
                // The idle posture is a list of its own; the caller's list is not edited.
                let result = self.send_pt(&mut native::list_of(&IDLE_POSTURE), ctx);
                self.edited = None;
                result.map_err(call(215))
            }
            0 => self.send_pt_compat(motors, ctx).map_err(call(217)),
            _ => Ok(()),
        }
    }

    /// A test frame on the caller's list; the idle posture is a list of its own.
    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        if !ctx.link.is_connected() {
            return Ok(());
        }
        let settings = self.settings.get();
        // Test mode 1, test mode 0 with no game (the idle posture) and test mode 0 with a game each
        // send the position frames from a frame line of their own.
        let call = |line: u32| move |e: MotionError| e.through(PyFrame::new(PTCAN_FILE, "send_test", line));
        match settings.motion_test_mode {
            1 => self.send_pt(motors, ctx).map_err(call(212)),
            0 if settings.no_game_running => self.send_pt(&mut native::list_of(&IDLE_POSTURE), ctx).map_err(call(215)),
            0 => self.send_pt(motors, ctx).map_err(call(217)),
            _ => Ok(()),
        }
    }

    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.actuators_travel().map(CornerTravel::to_positions)
    }

    fn actuators_travel(&self) -> Option<CornerTravel> {
        self.vertical_positions.map(CornerTravel::from)
    }

    fn take_edited_frame(&mut self) -> Option<Vec<i64>> {
        self.edited.take()
    }

    fn describe(&self) -> String {
        format!("PT-Actuator CANBus on {}", self.settings.get().pt_com_port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::testing::FakeHost;
    use crate::motion::{motion_model, LinkState, MotionHost};

    const MODEL: &str = "6 Actuators (2 Front + 2 Rear + TL + Surge) CANBus";
    const PORT: &str = "/dev/ttyUSB7";

    fn settings() -> PtCanSettings {
        PtCanSettings {
            pt_com_port: PORT.into(),
            baud_rate: Ok(LegacyValue::Int(115_200)),
            parking_speed: Ok(LegacyValue::Int(1000)),
            reaction_speed_map_min: Ok(LegacyValue::Int(1000)),
            reaction_speed_map_max: Ok(LegacyValue::Int(8192)),
            park_to_motion_speed_delay_s: Ok(LegacyValue::Int(5)),
            reverse_ports: [false; 3],
            reaction_speed: Ok(LegacyValue::Int(100)),
            compensation_stroke_mm: Ok(LegacyValue::Int(100)),
            vehicle_speed: 10.0,
            motion_test_mode: 0,
            no_game_running: true,
            runtime: Default::default(),
        }
    }

    type Timers = Arc<std::sync::Mutex<Vec<(Duration, Box<dyn FnOnce() + Send>)>>>;

    struct Rig {
        driver: PtCanDriver,
        settings: LiveSettings<PtCanSettings>,
        warning: MotionWarning,
        host: FakeHost,
        link: LinkState,
        /// The timers started, not yet run.
        timers: Timers,
    }

    impl Rig {
        fn new(s: PtCanSettings) -> Self {
            let settings = LiveSettings::new(s);
            let warning = MotionWarning::new("Cannot connect to Yaw VR".into());
            let timers = Timers::default();
            let started = Arc::clone(&timers);
            let driver = PtCanDriver::new(settings.clone(), warning.clone())
                .with_timer(Box::new(move |interval, function| started.lock().unwrap().push((interval, function))));
            Rig { driver, settings, warning, host: FakeHost::default(), link: LinkState::default(), timers }
        }

        /// Runs the oldest pending timer (its 5 s are over).
        fn fire_timer(&mut self) {
            let (interval, function) = self.timers.lock().unwrap().remove(0);
            assert_eq!(interval, WAIT_MODE_DURATION);
            function();
        }
        fn call<R>(&mut self, f: impl FnOnce(&mut PtCanDriver, &mut MotionContext<'_>) -> R) -> R {
            let mut ctx = MotionContext {
                model: motion_model(MODEL, false).unwrap(),
                link: &mut self.link,
                host: &mut self.host,
            };
            f(&mut self.driver, &mut ctx)
        }
        fn writes(&mut self) -> Vec<Vec<u8>> {
            self.host.log(PORT).written()
        }
    }

    /// The homing and position frame byte layouts (speed high byte twice).
    #[test]
    fn frame_layouts() {
        assert_eq!(findz_frame(3, 1000), [0, 3, 0, 0x1A, 0x07, 0x03, 0x03, 0x53, 0, 0, 0x0D, 0x0A]);
        assert_eq!(position_frame(7, 0x1234, 0x0ABC), [0, 7, 0, 0x1A, 0x1D, 0x0A, 0x0A, 0x05, 0x12, 0x34, 0x0D, 0x0A]);
        assert_eq!(position_frame(1, 65536, 500)[8..10], [0, 0], "c_uint16 wraps");
        assert_eq!(hex_string(&[0x0A, 0xFF]), "0A FF");
    }

    /// Connect: find-z to ids 1..=7 at the sanitised parking speed, the configured pause, then Connected;
    /// the port is opened with the configured baud rate and blocking writes.
    #[test]
    fn connect_homes_every_can_id() {
        let mut rig = Rig::new(PtCanSettings { parking_speed: Ok(LegacyValue::Int(9000)), ..settings() });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.is_connected());
        let w = rig.writes();
        assert_eq!(w.len(), 7);
        for (i, frame) in w.iter().enumerate() {
            assert_eq!(frame, &findz_frame(i as u8 + 1, 500).to_vec());
        }
        assert_eq!(rig.host.now(), Duration::from_secs(5));
        assert_eq!(rig.host.log(PORT).opens[0], OpenOptions { baud_rate: 115_200, write_timeout: None });
    }

    /// Connect: a negative park_to_motion_speed_delay_s is an error; the connect fails before
    /// 'Connected' and the wait mode.
    #[test]
    fn connect_negative_delay_fails() {
        let mut rig = Rig::new(PtCanSettings { park_to_motion_speed_delay_s: Ok(LegacyValue::Int(-1)), ..settings() });
        assert!(rig.call(|d, ctx| d.connect(ctx)).is_err());
        assert!(!rig.link.is_connected());
        assert_eq!(rig.writes().len(), 7, "find-z frames were sent before the sleep");
        assert_eq!(rig.host.now(), Duration::ZERO);
    }

    /// Connect: motion port entry 0 is assigned first. A port that cannot be opened, or a baud rate
    /// that is not an integer,
    /// ends the connect quietly: no error reaches the caller.
    #[test]
    fn connect_open_and_baud_failures_return_quietly() {
        use crate::motion::driver_support::testing::capture_diagnostics;
        use crate::motion::testing::Device;

        let mut busy = Rig::new(settings());
        busy.host = std::mem::take(&mut busy.host).device(PORT, Device { fail_open: true, ..Device::default() });
        let mut no_baud = Rig::new(PtCanSettings {
            baud_rate: Err("ValueError: invalid literal for int() with base 10: 'x'".into()),
            ..settings()
        });
        for rig in [&mut busy, &mut no_baud] {
            let mut result = None;
            let diagnostics = capture_diagnostics(|| result = Some(rig.call(|d, ctx| d.connect(ctx))));
            result.unwrap().expect("the handler returns False");
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            assert!(!rig.link.is_connected());
            assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(PORT), "L70 ran before the try");
            assert_eq!(rig.warning.get(), "Cannot connect to Yaw VR");
            assert!(rig.timers.lock().unwrap().is_empty());
            assert!(rig.writes().is_empty());
        }
        assert!(no_baud.host.log(PORT).opens.is_empty());
    }

    /// Connect: each find-z frame is logged (`<id> find z: <hex>`) before it is written; a
    /// failed write logs `Failed to findz <error>` after it and the loop goes on.
    #[test]
    fn connect_logs_each_findz_frame_before_writing_it() {
        use crate::motion::driver_support::testing::capture_debug_messages;
        use crate::motion::testing::Device;

        let mut rig = Rig::new(settings());
        let second = findz_frame(2, 1000).to_vec();
        rig.host = std::mem::take(&mut rig.host).device(PORT, Device { fail_on: vec![second], ..Device::default() });
        let messages = capture_debug_messages(|| rig.call(|d, ctx| d.connect(ctx)).unwrap());
        let expected: Vec<String> = (1..=7u8)
            .flat_map(|can_id| {
                let logged = format!("{can_id} find z: {}", hex_string(&findz_frame(can_id, 1000)));
                let failed = (can_id == 2).then(|| "Failed to findz Write timeout".to_string());
                std::iter::once(logged).chain(failed)
            })
            .collect();
        assert_eq!(messages, expected);
        assert!(rig.link.is_connected());
        assert_eq!(rig.writes().len(), 6);
    }

    /// Connect: an empty pt_com_port does nothing.
    #[test]
    fn connect_without_port_is_a_no_op() {
        let mut rig = Rig::new(PtCanSettings { pt_com_port: String::new(), ..settings() });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(!rig.link.is_connected() && rig.link.ports.is_empty());
    }

    /// Position frames: parking speed during the 5 s wait mode, then the reaction-speed map; 1000 at standstill.
    #[test]
    fn frame_speed_follows_wait_mode_and_vehicle_speed() {
        let mut rig = Rig::new(PtCanSettings { reaction_speed: Ok(LegacyValue::Int(85)), ..settings() });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        let motors = [100, 200, 300, 400, 500, 600, 700, 800, 0, 0, 0, 0];
        rig.call(|d, ctx| d.send_stream(&motors, ctx)).unwrap();
        let w = rig.writes();
        assert_eq!(w[7], position_frame(1, 100, 1000).to_vec(), "wait mode: parking speed 1000");
        assert_eq!(w[13], position_frame(7, 700, 1000).to_vec());
        assert_eq!(w.len(), 14, "ids 1..=7 only");
        rig.fire_timer();
        rig.call(|d, ctx| d.send_stream(&motors, ctx)).unwrap();
        // 85 rescaled from 70..100 to 1000..8192 is 4596
        assert_eq!(rig.writes()[14], position_frame(1, 100, 4596).to_vec());
        rig.settings.update(|s| s.vehicle_speed = 0.0);
        rig.call(|d, ctx| d.send_stream(&motors, ctx)).unwrap();
        assert_eq!(rig.writes()[21], position_frame(1, 100, 1000).to_vec());
    }

    /// Out-of-range map bounds fall back to 1000 and 8192.
    #[test]
    fn map_bounds_fallbacks() {
        let mut rig = Rig::new(PtCanSettings {
            reaction_speed_map_min: Ok(LegacyValue::Int(-1)),
            reaction_speed_map_max: Ok(LegacyValue::Int(9000)),
            reaction_speed: Ok(LegacyValue::Int(70)),
            ..settings()
        });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.fire_timer();
        rig.call(|d, ctx| d.send_stream(&[0; 12], ctx)).unwrap();
        assert_eq!(rig.writes()[7], position_frame(1, 0, 1000).to_vec());
    }

    /// Reverse flags on slots 4..6 (65536 - v), actuator positions from the MOTION_COMPENSATION stroke.
    #[test]
    fn reverse_flags_and_positions() {
        let mut rig = Rig::new(PtCanSettings { reverse_ports: [true, false, true], ..settings() });
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        let motors = [0, 65535, 65535, 0, 1000, 2000, 0, 0];
        rig.call(|d, ctx| d.send_stream(&motors, ctx)).unwrap();
        let w = rig.writes();
        assert_eq!(w[11], position_frame(5, 64536, 1000).to_vec());
        assert_eq!(w[12], position_frame(6, 2000, 1000).to_vec());
        assert_eq!(w[13], position_frame(7, 65536, 1000).to_vec());
        let p = rig.driver.vertical_positions().unwrap();
        assert_eq!((p.front_left, p.rear_left, p.rear_right), (100.0, 0.0, 0.0));
        assert_eq!(rig.driver.actuators_travel().unwrap().front_left, 100.0);
        // The reverse switches edit the caller's list in place (slot 4 here).
        let edited = rig.driver.take_edited_frame().unwrap();
        assert_eq!(&edited[4..7], &[64536, 2000, 65536]);
    }

    /// A model with fewer than seven slots is an index error in the frame loop.
    #[test]
    fn short_motor_list_is_an_error() {
        let mut rig = Rig::new(settings());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.call(|d, ctx| d.send_stream(&[0; 4], ctx)).is_err());
    }

    /// Test frames: motion_test_mode 0 sends the idle posture (four times 3276, eight times 32768) when no game runs and the
    /// given frame while a game runs; 1 sends the given frame; any other value sends nothing.
    #[test]
    fn send_test_by_motion_test_mode() {
        let mut rig = Rig::new(settings());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.call(|d, ctx| d.send_test(&[1; 12], ctx)).unwrap();
        let w = rig.writes();
        assert_eq!(w[7], position_frame(1, 3276, 1000).to_vec());
        assert_eq!(w[11], position_frame(5, 32768, 1000).to_vec());
        rig.settings.update(|s| s.no_game_running = false);
        rig.call(|d, ctx| d.send_test(&[2; 12], ctx)).unwrap();
        assert_eq!(rig.writes().len(), 21);
        assert_eq!(rig.writes()[14], position_frame(1, 2, 1000).to_vec());
        rig.settings.update(|s| s.motion_test_mode = 1);
        rig.call(|d, ctx| d.send_test(&[3; 12], ctx)).unwrap();
        assert_eq!(rig.writes()[21], position_frame(1, 3, 1000).to_vec());
        rig.settings.update(|s| {
            s.motion_test_mode = 2;
            s.no_game_running = true;
        });
        rig.call(|d, ctx| d.send_test(&[4; 12], ctx)).unwrap();
        assert_eq!(rig.writes().len(), 28, "other values send nothing");
    }

    /// Connect: the motion warning is cleared after 'Connected'; a failed connect leaves it.
    #[test]
    fn connect_clears_motion_warning() {
        let mut rig = Rig::new(PtCanSettings { park_to_motion_speed_delay_s: Ok(LegacyValue::Int(-1)), ..settings() });
        assert!(rig.call(|d, ctx| d.connect(ctx)).is_err());
        assert_eq!(rig.warning.get(), "Cannot connect to Yaw VR");
        rig.settings.update(|s| s.park_to_motion_speed_delay_s = Ok(LegacyValue::Int(0)));
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.warning.get().is_empty());
    }

    /// A write error inside the CAN-id loop: 'Not Connected' (capital C),
    /// motion port entry 0 removed, port closed; the loop goes on, the next write fails on the closed port and
    /// the second removal of entry 0 is a key error that ends the pass.
    #[test]
    fn write_error_disconnects() {
        let mut rig = Rig::new(settings());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.host.devices.entry(PORT.into()).or_default().fail_writes = true;
        rig.driver.port =
            Some(rig.host.open_serial(PORT, &OpenOptions { baud_rate: 115_200, write_timeout: None }).unwrap());
        let error = rig.call(|d, ctx| d.send_stream(&[0; 12], ctx)).unwrap_err();
        assert!(error.to_string().contains("KeyError"), "{error}");
        assert_eq!(rig.link.status, LinkStatus::NotConnectedAfterWriteError);
        assert!(!rig.link.is_connected() && rig.link.ports.is_empty());
        assert!(rig.driver.port.is_none());
    }

    /// The second removal of motion port entry 0 is a key error whose text is `0`.
    #[test]
    fn write_error_key_error_reads_as_python() {
        let mut rig = Rig::new(settings());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.host.devices.entry(PORT.into()).or_default().fail_writes = true;
        rig.driver.port =
            Some(rig.host.open_serial(PORT, &OpenOptions { baud_rate: 115_200, write_timeout: None }).unwrap());
        let error = rig.call(|d, ctx| d.send_stream(&[0; 12], ctx)).unwrap_err();
        assert_eq!(error.python_str(), "0");
        assert_eq!(error.to_string(), "KeyError: 0");
        let frames: Vec<(&str, u32)> = error.frames().iter().map(|frame| (frame.function, frame.line)).collect();
        assert_eq!(frames, [("send_stream", 205), ("send_pt", 187)]);
    }

    /// A write error on the last CAN id ends the loop normally: the pass returns without an error.
    #[test]
    fn write_error_on_the_last_can_id_returns_normally() {
        let mut rig = Rig::new(settings());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        let mut device = crate::motion::testing::Device::default();
        let speed = rig.driver.frame_speed(&rig.settings.get()).unwrap();
        let last = position_frame(CAN_IDS.end - 1, 0, crate::motion::native::as_i64(&speed).unwrap());
        device.fail_on.push(last.to_vec());
        rig.host.devices.insert(PORT.into(), device);
        rig.driver.port =
            Some(rig.host.open_serial(PORT, &OpenOptions { baud_rate: 115_200, write_timeout: None }).unwrap());
        rig.call(|d, ctx| d.send_stream(&[0; 12], ctx)).unwrap();
        assert_eq!(rig.link.status, LinkStatus::NotConnectedAfterWriteError);
        assert!(rig.link.ports.is_empty());
    }

    /// The wait mode is on at start; every connect sets it and starts a 5 s timer; timers are not
    /// cancelled, so the one of an earlier connect clears it early.
    #[test]
    fn wait_mode_timers_are_not_cancelled() {
        let mut rig = Rig::new(settings());
        assert!(rig.driver.wait_mode(), "import-time True");
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert_eq!(rig.timers.lock().unwrap().len(), 2);
        assert!(rig.driver.wait_mode());
        rig.fire_timer();
        assert!(!rig.driver.wait_mode(), "the first timer clears it");
    }

    fn native_list(values: Vec<LegacyValue>) -> LegacyValue {
        LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(values)))
    }

    /// Position frames on the caller's dynamic list: the reversed slots are written into the shared object (also
    /// while not connected, where nothing else happens), and every position is reduced to 16 bits.
    #[test]
    fn native_reversal_is_written_and_positions_are_masked() {
        let mut rig = Rig::new(PtCanSettings { reverse_ports: [true, false, false], ..settings() });
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 70);
        let mut values: Vec<LegacyValue> = (0..12).map(|_| native::int(32768)).collect();
        values[4] = wide.clone();
        let mut list = native_list(values);
        let alias = list.clone();
        rig.driver
            .send_pt(
                &mut list,
                &mut MotionContext {
                    model: motion_model(MODEL, false).unwrap(),
                    link: &mut rig.link,
                    host: &mut rig.host,
                },
            )
            .unwrap();
        let reversed = native::sub(&native::int(65_536), &wide).unwrap();
        assert_eq!(native::item(&alias, 4).unwrap(), reversed);
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        let before = rig.writes().len();
        rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap();
        // Reversed again: 65536 - (65536 - 2**70) = 2**70, whose low 16 bits are 0.
        assert_eq!(native::item(&alias, 4).unwrap(), wide);
        let frames = rig.writes();
        assert_eq!(frames.len(), before + 7);
        assert_eq!(frames[before + 4][8..10], [0, 0]);
    }

    /// The frame speed compares `0 <= value <= 8192` at any width and rescales the integer reaction speed as a 64-bit float.
    #[test]
    fn native_speed_settings_keep_their_width_until_the_comparison() {
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 70);
        let rig = Rig::new(PtCanSettings { reaction_speed_map_max: Ok(wide.clone()), ..settings() });
        rig.driver.wait_mode.0.store(false, Ordering::SeqCst);
        // 2**70 is outside 0..=8192, so the default 8192 is the map bound: reaction 100 → 8192.
        assert_eq!(rig.driver.frame_speed(&rig.settings.get()).unwrap(), native::int(8192));
        let huge = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 1100);
        let rig = Rig::new(PtCanSettings { reaction_speed: Ok(huge), ..settings() });
        rig.driver.wait_mode.0.store(false, Ordering::SeqCst);
        let error = rig.driver.frame_speed(&rig.settings.get()).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "OverflowError: int too large to convert to float");
        let rig = Rig::new(PtCanSettings { park_to_motion_speed_delay_s: Ok(wide), ..settings() });
        let mut rig = rig;
        let error = rig.call(|d, ctx| d.connect(ctx)).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "OverflowError: timestamp too large to convert to C PyTime_t");
        let mut rig = Rig::new(PtCanSettings { park_to_motion_speed_delay_s: Ok(native::int(-1)), ..settings() });
        let error = rig.call(|d, ctx| d.connect(ctx)).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "ValueError: sleep length must be non-negative");
    }

    /// The corner entries are computed one per frame line (for the four-corner layout): a slot
    /// too wide for a 64-bit float fails inside the rescale there, before any frame is written.
    #[test]
    fn native_corner_map_errors_carry_their_entry_line() {
        let mut rig = Rig::new(settings());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        let before = rig.writes().len();
        let mut values: Vec<LegacyValue> = (0..12).map(|_| native::int(32768)).collect();
        values[1] = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 1100);
        let mut list = native_list(values);
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 205), ("send_pt", 146), ("map", 204)]);
        assert_eq!(rig.writes().len(), before);
    }

    /// `PT-ACTUATOR.pt_com_port` is read at each access of the connect (the emptiness test).
    #[test]
    fn native_pt_com_port_is_read_live_at_connect() {
        use crate::motion::driver_support::testing::FakeConfig;
        let mut rig = Rig::new(PtCanSettings { pt_com_port: String::new(), ..settings() });
        let config = std::sync::Arc::new(FakeConfig::default());
        rig.settings.read(|s| s.runtime.install_config(config.clone()));
        for (key, value) in
            [("pt_com_baud_rate", 115_200), ("parking_speed", 1000), ("park_to_motion_speed_delay_s", 0)]
        {
            config.set("PT-ACTUATOR", key, native::int(value));
        }
        config.set("PT-ACTUATOR", "pt_com_port", LegacyValue::from(""));
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(rig.link.ports.is_empty(), "an empty live port does not connect");
        config.set("PT-ACTUATOR", "pt_com_port", LegacyValue::from(PORT));
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(PORT));
        assert!(rig.link.is_connected());
    }
}

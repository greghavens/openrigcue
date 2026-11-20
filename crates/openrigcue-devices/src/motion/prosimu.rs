//! PROSIMU platforms with PRS200 / PRS50 boards, optionally with Dyadic SCN6 traction-loss/surge axes.
//!
//! One driver type serves four variants, selected by the model's controller key:
//! `prs200` (Dyadic), `prs200_sv` (SV), `prs200_tl_surge` (traction loss and surge)
//! and `prs200_prs50` (PRS50).
//!
//! - [`board`]: PRS200 frames and the 19200 8N2 board ports.
//! - [`crc16`][]: CRC-16/XMODEM.
//! - [`reaction`]: the speed/acceleration ("reaction speed") profile and the start-up sends.

pub mod board;
pub mod crc16;
pub mod reaction;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use openrigcue_model::native_value::LegacyValue;
use tracing::{debug, warn};

use self::board::{BoardPorts, Prs200Board, SpeedLayout, SystemBoardPorts};
use self::reaction::{game_state, init_reaction_speed_live, Pair, ReactionSpeed, Style, IDLE};
use super::clock::{Clock, SystemClock};
use super::driver_support::{boolean_setting_error, CornerTravel, HostActivity};
use super::native::{self, NativeSetting};
use super::scn6::axis::{
    assigned_id, assigned_key, move_org, ports_repr, scan_records, wait_for_homing, ActuatorRecord, ScanModule, AXIS,
};
use super::scn6::tmbs::Command;
use super::{python, python_print};
use super::{
    ActuatorPositions, Controller, DriverFactory, LinkStatus, LiveSettings, ModelSpec, MotionContext, MotionDriver,
    MotionError, PyFrame,
};

/// The largest motor value of the PRS200 models (the TL position is scaled from this range).
const FULL_SCALE: f64 = 65536.0;
/// TL/surge stroke in Dyadic increments: the slot rescaled from 0..65536 to 0..10000, negated.
const DYADIC_STROKE: f64 = 10000.0;
/// Speed and acceleration of the Dyadic axes in these variants.
const DYADIC_SPEED: i64 = 5000;
const DYADIC_ACCEL: i64 = 80;
/// Homing mode sent to the Dyadic axes.
const DYADIC_HOMING_MODE: &str = "07";
/// Traction-loss slot.
const TL_SLOT: usize = 2;
/// A test frame lowers the corner actuators to this value while idle.
const IDLE_TEST_POSITION: i64 = 250;
/// A test frame centres the traction-loss slot to this value while idle (all variants except PRS50,
/// which sends 0).
const IDLE_TEST_TL_CENTRE: i64 = 32768;
/// The TL/surge variant's mapping: assignment option → motor slot (TL → 2, Surge → 5).
const TL_SURGE_SLOTS: [(i64, usize); 2] = [(0, 2), (1, 5)];
/// The PRS50 model whose second board drives TL and surge.
const SV_RK: &str = "T1000-6M SV RK";

/// Which variant of the PROSIMU driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prs200Module {
    Dyadic,
    Sv,
    TlSurge,
    Prs50,
}

impl Prs200Module {
    pub fn of(controller: Controller) -> Option<Self> {
        match controller {
            Controller::Prs200 => Some(Prs200Module::Dyadic),
            Controller::Prs200Sv => Some(Prs200Module::Sv),
            Controller::Prs200TlSurge => Some(Prs200Module::TlSurge),
            Controller::Prs200Prs50 => Some(Prs200Module::Prs50),
            _ => None,
        }
    }

    /// Whether the variant has Dyadic axes at all (PRS50 has none).
    fn has_dyadic(self) -> bool {
        self != Prs200Module::Prs50
    }

    /// The file name in the frames of the variant's errors.
    fn file(self) -> &'static str {
        match self {
            Prs200Module::Dyadic => "backend/controllers/prs200_dyadic.py",
            Prs200Module::Sv => "backend/controllers/prs200_dyadic_sv.py",
            Prs200Module::TlSurge => "backend/controllers/prs200_dyadic_tl_surge.py",
            Prs200Module::Prs50 => "backend/controllers/prs200_prs50.py",
        }
    }

    /// The frame of the variant's connect at `line`.
    fn connect_frame(self, line: u32) -> PyFrame {
        PyFrame::new(self.file(), "connect", line)
    }

    /// The frame lines of the steps of the variant's connect whose errors leave it.
    fn connect_lines(self) -> ConnectLines {
        match self {
            Prs200Module::Dyadic => ConnectLines {
                enable: [113, 120],
                tl_port: 238,
                second_port: 0,
                start_scan: 0,
                port: [270, 290],
                is_connected: [279, 299],
            },
            Prs200Module::Sv => ConnectLines {
                enable: [113, 120],
                tl_port: 238,
                second_port: 0,
                start_scan: 267,
                port: [271, 291],
                is_connected: [280, 300],
            },
            Prs200Module::TlSurge => ConnectLines {
                enable: [109, 118],
                tl_port: 242,
                second_port: 265,
                start_scan: 0,
                port: [278, 298],
                is_connected: [287, 307],
            },
            Prs200Module::Prs50 => ConnectLines {
                enable: [0, 0],
                tl_port: 0,
                second_port: 0,
                start_scan: 0,
                port: [106, 126],
                is_connected: [115, 135],
            },
        }
    }

    /// The frame lines of the steps of a stream frame (or a test frame) whose errors leave it.
    fn frame_lines(self, test: bool) -> FrameLines {
        let (function, [check_reaction_speed, enable, send_actuator_pos, stroke]) = match (self, test) {
            (Prs200Module::Dyadic, false) => ("send_stream", [487, 489, 490, 492]),
            (Prs200Module::Dyadic, true) => ("send_test", [526, 535, 536, 0]),
            (Prs200Module::Sv, false) => ("send_stream", [523, 525, 526, 528]),
            (Prs200Module::Sv, true) => ("send_test", [563, 572, 573, 0]),
            (Prs200Module::TlSurge, false) => ("send_stream", [545, 552, 553, 547]),
            (Prs200Module::TlSurge, true) => ("send_test", [581, 590, 591, 0]),
            (Prs200Module::Prs50, false) => ("send_stream", [302, 0, 0, 304]),
            (Prs200Module::Prs50, true) => ("send_test", [340, 0, 0, 0]),
        };
        FrameLines { function, check_reaction_speed, enable, send_actuator_pos, stroke }
    }

    /// The frame lines of the variant's reaction-speed check, its target calculation and the board's speed command.
    fn reaction_lines(self) -> ReactionLines {
        match self {
            Prs200Module::Dyadic => ReactionLines {
                reads: [[461, 463]; 2],
                calc_call: None,
                send: [477, 479],
                class_model: 76,
                class_write: 81,
            },
            Prs200Module::Sv => ReactionLines {
                reads: [[460, 462], [465, 467]],
                calc_call: Some(501),
                send: [513, 515],
                class_model: 76,
                class_write: 81,
            },
            Prs200Module::TlSurge => ReactionLines {
                reads: [[519, 521]; 2],
                calc_call: None,
                send: [535, 537],
                class_model: 0,
                class_write: 77,
            },
            Prs200Module::Prs50 => ReactionLines {
                reads: [[240, 242], [245, 247]],
                calc_call: Some(281),
                send: [292, 294],
                class_model: 69,
                class_write: 74,
            },
        }
    }

    /// A frame of the variant at `line` of `function`.
    fn at(self, function: &'static str, line: u32) -> PyFrame {
        PyFrame::new(self.file(), function, line)
    }

    /// The frame lines of the variant's position pass, velocity write and id lookup.
    fn axis_lines(self) -> AxisLines {
        match self {
            Prs200Module::Dyadic => AxisLines {
                invert: 341,
                write_first: 364,
                write_changed: 372,
                get_id: 0,
                mapping: 0,
                position_map: 376,
                write_velocity: 96,
            },
            Prs200Module::Sv => AxisLines {
                invert: 342,
                write_first: 365,
                write_changed: 373,
                get_id: 0,
                mapping: 0,
                position_map: 377,
                write_velocity: 96,
            },
            Prs200Module::TlSurge | Prs200Module::Prs50 => AxisLines {
                invert: 388,
                write_first: 413,
                write_changed: 420,
                get_id: 422,
                mapping: 428,
                position_map: 436,
                write_velocity: 92,
            },
        }
    }

    /// The variant as the shared SCN6 scan knows it; `None` for PRS50, which has no scan.
    fn scan_module(self) -> Option<ScanModule> {
        match self {
            Prs200Module::Dyadic => Some(ScanModule::Prs200),
            Prs200Module::Sv => Some(ScanModule::Prs200Sv),
            Prs200Module::TlSurge => Some(ScanModule::Prs200TlSurge),
            Prs200Module::Prs50 => None,
        }
    }

    /// The variant's SV model-name test, used by its speed layout and reaction profile.
    fn sv_listed(self, model: &str) -> bool {
        const SV: [&str; 4] = ["T1000-2M SV", "T1000-3M SV", "T1000-5M SV", "P5MP SV"];
        match self {
            Prs200Module::Sv | Prs200Module::Dyadic => SV.contains(&model),
            Prs200Module::Prs50 => SV.contains(&model) || model == SV_RK,
            Prs200Module::TlSurge => false,
        }
    }

    fn speed_layout(self, model: &str) -> SpeedLayout {
        if self.sv_listed(model) {
            SpeedLayout::AccelSpeedPairs
        } else {
            SpeedLayout::Mode4
        }
    }

    fn speed_layout_native(self, model: &Result<LegacyValue, String>) -> Result<SpeedLayout, MotionError> {
        if self == Prs200Module::TlSurge {
            return Ok(SpeedLayout::Mode4);
        }
        let model = model.as_ref().map_err(|detail| MotionError::Native(detail.clone()))?;
        Ok(self.speed_layout(model.as_str().unwrap_or("")))
    }

    fn reaction_style_native(self, model: &Result<LegacyValue, String>) -> Result<Style, MotionError> {
        if matches!(self, Prs200Module::Dyadic | Prs200Module::TlSurge) {
            return Ok(Style::Classic);
        }
        let model = model.as_ref().map_err(|detail| MotionError::Native(detail.clone()))?;
        Ok(self.reaction_style(model.as_str().unwrap_or("")))
    }

    /// Whether the variant treats a game without a running listener as idle.
    fn tests_listener(self) -> bool {
        matches!(self, Prs200Module::Dyadic | Prs200Module::TlSurge)
    }

    fn reaction_style(self, model: &str) -> Style {
        match self {
            Prs200Module::Dyadic | Prs200Module::TlSurge => Style::Classic,
            Prs200Module::Sv | Prs200Module::Prs50 => Style::Sv { sv_model: self.sv_listed(model) },
        }
    }

    /// The motion port keys of boards A and B.
    fn board_keys(self) -> [usize; 2] {
        match self {
            Prs200Module::TlSurge => [2, 3],
            _ => [1, 2],
        }
    }

    /// The traction-loss value of the idle test frame.
    fn idle_test_tl(self) -> i64 {
        match self {
            Prs200Module::Prs50 => 0,
            Prs200Module::Dyadic | Prs200Module::Sv | Prs200Module::TlSurge => IDLE_TEST_TL_CENTRE,
        }
    }

    /// Whether the reaction-speed check tests board A before sending to it. Only
    /// the SV variant does; the others send to board A unguarded, which is an error
    /// while board A is not open. Board B is tested in every variant.
    fn guards_board_a(self) -> bool {
        self == Prs200Module::Sv
    }
}

/// Frame lines of a variant's connect.
struct ConnectLines {
    /// The two reads of `PROSIMU.enable_dyatic_tl` as a boolean: the count adjustment and the scan test.
    enable: [u32; 2],
    /// The Dyadic TL port is set from the port of actuator record 0.
    tl_port: u32,
    /// Motion port entry 1 is set from the port of actuator record 1 (TL/surge variant).
    second_port: u32,
    /// The "start scan of the prs200a" line of the SV variant, which reads both port settings.
    start_scan: u32,
    /// The reads of `PROSIMU.prs200_com_port1` and `prs200_com_port2` inside the port loop.
    port: [u32; 2],
    /// The connection tests of boards A and B.
    is_connected: [u32; 2],
}

/// Frame lines of a variant's stream frame or test frame.
struct FrameLines {
    function: &'static str,
    /// The reaction-speed check, the first step of both.
    check_reaction_speed: u32,
    /// The test for more than one controller and an enabled Dyadic TL.
    enable: u32,
    /// The position pass of the Dyadic axes.
    send_actuator_pos: u32,
    /// The first read of the vertical stroke setting for the actuator positions.
    stroke: u32,
}

/// Frame lines of a variant's reaction-speed update.
struct ReactionLines {
    /// The two reads of the reaction speed setting in the order they are made, for a listed SV
    /// model and for any other: in the check itself, or in the target calculation for the
    /// variants that have one (called at `calc_call` of the check).
    reads: [[u32; 2]; 2],
    calc_call: Option<u32>,
    /// The speed command sends to board A and to board B.
    send: [u32; 2],
    /// The model setting read inside the board's speed command (0: the variant has none).
    class_model: u32,
    /// The serial write inside the board's speed command.
    class_write: u32,
}

/// Frame lines of a variant's position pass, and of the helpers it calls.
struct AxisLines {
    /// The read of `PROSIMU.invert_tl` as a boolean.
    invert: u32,
    /// The velocity write after the first lookup.
    write_first: u32,
    /// The velocity write of a changed pair.
    write_changed: u32,
    /// The id lookup of an actuator (TL/surge variant).
    get_id: u32,
    /// The motor slot looked up for the id.
    mapping: u32,
    /// The rescale of the slot from 0..65536 to 0..10000 in the position loop.
    position_map: u32,
    /// The protocol's velocity write inside the variant's velocity write.
    write_velocity: u32,
}

/// The frame line of the key's integer conversion inside the TL/surge variant's id lookup.
const TL_SURGE_GET_ID_INT: u32 = 339;

/// The error of a speed command to a board that was never opened.
const NO_BOARD: &str = "AttributeError: 'NoneType' object has no attribute 'send_speed_and_acceleration'";

fn board_a_missing() -> MotionError {
    MotionError::Native(NO_BOARD.into())
}

/// The port-not-open error of a write to a closed board's port.
fn board_closed() -> MotionError {
    MotionError::Link(crate::serial::LinkError::Io(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "Attempting to use a port that is not open",
    )))
}

/// One PRS200 board slot (A or B): never opened, a board whose port was closed, or an open board.
#[derive(Default)]
enum BoardSlot {
    #[default]
    Empty,
    Closed,
    Open(Prs200Board),
}

impl BoardSlot {
    fn open(&mut self) -> Option<&mut Prs200Board> {
        match self {
            BoardSlot::Open(board) => Some(board),
            _ => None,
        }
    }

    #[cfg(test)]
    fn is_open(&self) -> bool {
        matches!(self, BoardSlot::Open(_))
    }

    /// A disconnect closes the port; the board stays in its slot.
    fn close(&mut self) {
        if !matches!(self, BoardSlot::Empty) {
            *self = BoardSlot::Closed;
        }
    }

    /// The speed command on the slot: a board never opened is an attribute error, a closed board
    /// the port-not-open error.
    fn send_speed(&mut self, layout: SpeedLayout, pair: Pair) -> Result<(), MotionError> {
        match self {
            BoardSlot::Open(board) => board.send_speed(layout, pair.speed, pair.accel),
            BoardSlot::Closed => Err(board_closed()),
            BoardSlot::Empty => Err(board_a_missing()),
        }
    }

    fn send_speed_current(
        &mut self,
        module: Prs200Module,
        settings: &LiveSettings<ProsimuSettings>,
        pair: Pair,
    ) -> Result<(), MotionError> {
        if matches!(self, BoardSlot::Empty) {
            return Err(board_a_missing());
        }
        // The speed command reads the model setting to pick the layout. (The
        // pairs computed here always fit a byte, so packing the frame cannot fail.)
        let lines = module.reaction_lines();
        let layout = module
            .speed_layout_native(&settings.get().model_now())
            .map_err(|e| e.in_frame(module.at("PRS200.send_speed_and_acceleration", lines.class_model)))?;
        // Its serial write: a closed board (or a write that times out) is a serial error with its own frame.
        let write = module.at("PRS200.send_speed_and_acceleration", lines.class_write);
        self.send_speed(layout, pair).map_err(|e| e.in_serial_call(super::SerialCall::Write, write))
    }
}

/// The two board slots, used by the frames and by the start-up threads.
#[derive(Default)]
struct Boards {
    a: BoardSlot,
    b: BoardSlot,
}

type SharedBoards = Arc<Mutex<Boards>>;

fn lock_boards(boards: &SharedBoards) -> MutexGuard<'_, Boards> {
    boards.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The body of one start-up thread; it is given the thread's clock.
pub type StartupJob = Box<dyn FnOnce(&mut dyn Clock) + Send>;

/// Starts a start-up thread (named `init_reaction_speed`).
pub type StartupSpawner = Box<dyn FnMut(StartupJob) + Send>;

/// The real spawner: a detached thread on the wall clock. Every connect starts one; an earlier one still
/// running goes on next to it.
fn spawn_startup_thread(job: StartupJob) {
    let spawned =
        std::thread::Builder::new().name("init_reaction_speed".into()).spawn(move || job(&mut SystemClock::default()));
    if let Err(e) = spawned {
        warn!("cannot start init_reaction_speed: {e}");
    }
}

/// The selected start-up branch's reaction-speed read. The thread's own start frames
/// are omitted, as in the daemon's thread formatter.
fn init_reaction_line(module: Prs200Module, style: Style, read: u32) -> u32 {
    let sv = matches!(style, Style::Sv { sv_model: true });
    let line = match module {
        Prs200Module::Dyadic => 420,
        Prs200Module::TlSurge => 478,
        Prs200Module::Sv => {
            if sv {
                427
            } else {
                430
            }
        }
        Prs200Module::Prs50 => {
            if sv {
                207
            } else {
                210
            }
        }
    };
    line + read.saturating_sub(1)
}

/// The reaction speed setting as the 64-bit float the rescale takes: the lookup and the integer
/// conversion fail at `frames`, the conversion of an integer too wide for a float inside the rescale's frame.
fn reaction_double(setting: &NativeSetting, frames: Vec<PyFrame>) -> Result<f64, MotionError> {
    let int = setting
        .clone()
        .map_err(MotionError::Native)
        .and_then(|raw| native::to_int(&raw))
        .map_err(|e| e.in_frames(frames.clone()))?;
    native::double(&int).map_err(|e| e.in_frame(native::FUNCTIONS_MAP).in_frames(frames))
}

fn init_reaction_failure(module: Prs200Module, line: u32, error: &MotionError) -> String {
    let source = match module {
        Prs200Module::Dyadic => "prs200_dyadic",
        Prs200Module::TlSurge => "prs200_dyadic_tl_surge",
        Prs200Module::Sv => "prs200_dyadic_sv",
        Prs200Module::Prs50 => "prs200_prs50",
    };
    let inner: String = error.frames().iter().map(PyFrame::text).collect();
    format!(
        "  File \"backend/controllers/{source}.py\", line {line}, in \
         backend.controllers.{source}.init_reaction_speed\n{inner}{error}\n"
    )
}

/// One start-up send: board A and (unless it was never opened) board B; an error of either is
/// handled on its own and printed as 'error writing speed and accel'; board A is called even when it is not open.
fn send_startup_pair(
    boards: &SharedBoards,
    module: Prs200Module,
    settings: &LiveSettings<ProsimuSettings>,
    pair: Pair,
) {
    let mut boards = lock_boards(boards);
    if boards.a.send_speed_current(module, settings, pair).is_err() {
        python_print(&["error writing speed and accel"]);
    }
    if !matches!(boards.b, BoardSlot::Empty) && boards.b.send_speed_current(module, settings, pair).is_err() {
        python_print(&["error writing speed and accel"]);
    }
}

/// The text of a raw setting value.
fn native_str(value: &LegacyValue) -> String {
    match value {
        LegacyValue::Str(text) => text.clone(),
        other => openrigcue_model::python_repr::repr(other),
    }
}

/// Whether a PROSIMU board model allows a change of the traction-loss direction.
fn allow_change_tl_dir(model: &str) -> bool {
    !matches!(model, "T1000-2M PRS200" | "T1000-2M SV" | "T1000AL-4M PRS200")
}

/// The `[PROSIMU]` and related settings, re-read on every call.
#[derive(Debug, Clone, PartialEq)]
pub struct ProsimuSettings {
    /// Where the settings are read live (the refreshed raw values are the fallback).
    pub runtime: super::driver_support::RuntimeReader,
    /// The current raw `[MOTIONBOX] model`, also read by workers left from earlier connections.
    pub model: Result<LegacyValue, String>,
    /// `prs200_com_port1` ("Vertical PRS200 / 50 Port").
    pub port1: Result<LegacyValue, String>,
    /// `prs200_com_port2` ("Horizontal PRS50 Port").
    pub port2: Result<LegacyValue, String>,
    /// `prs200_firmware` as an integer (default 3); a failed read is an error where the board is created.
    pub firmware: NativeSetting,
    /// `enable_dyatic_tl` (default yes).
    pub enable_dyadic_tl: Result<bool, String>,
    /// `invert_tl` (default no).
    pub invert_tl: Result<bool, String>,
    /// `[MOTIONBOX] reaction_speed` (slider 70..100).
    pub reaction_speed: NativeSetting,
    /// `[MOTION_COMPENSATION] vertical_actuators_stroke` (mm).
    pub vertical_stroke_mm: NativeSetting,
    /// The axis assignment (used by the TL/surge variant).
    pub axis_assignment: BTreeMap<String, String>,
}

impl Default for ProsimuSettings {
    fn default() -> Self {
        ProsimuSettings {
            runtime: Default::default(),
            model: Ok("None".into()),
            port1: Ok(LegacyValue::Str(String::new())),
            port2: Ok(LegacyValue::Str(String::new())),
            firmware: Ok(LegacyValue::Int(board::CRC_FIRMWARE)),
            enable_dyadic_tl: Ok(true),
            invert_tl: Ok(false),
            reaction_speed: Ok(LegacyValue::Int(70)),
            vertical_stroke_mm: Ok(LegacyValue::Int(0)),
            axis_assignment: BTreeMap::new(),
        }
    }
}

impl ProsimuSettings {
    /// `PROSIMU.enable_dyatic_tl` as a boolean, read at `frame`.
    fn dyadic_enabled(&self, frame: PyFrame) -> Result<bool, MotionError> {
        self.runtime
            .switch("PROSIMU", "enable_dyatic_tl", &self.enable_dyadic_tl)
            .map_err(|detail| boolean_setting_error(&detail, frame))
    }

    /// `MOTIONBOX.model` now.
    fn model_now(&self) -> NativeSetting {
        self.runtime.value("MOTIONBOX", "model", &self.model)
    }

    /// `PROSIMU.prs200_com_port1` now.
    fn port1_now(&self) -> NativeSetting {
        self.runtime.value("PROSIMU", "prs200_com_port1", &self.port1)
    }

    /// `PROSIMU.prs200_com_port2` now.
    fn port2_now(&self) -> NativeSetting {
        self.runtime.value("PROSIMU", "prs200_com_port2", &self.port2)
    }

    /// `PROSIMU.prs200_firmware` now.
    fn firmware_now(&self) -> NativeSetting {
        self.runtime.value("PROSIMU", "prs200_firmware", &self.firmware)
    }

    /// `MOTIONBOX.reaction_speed` now.
    fn reaction_now(&self) -> NativeSetting {
        self.runtime.value("MOTIONBOX", "reaction_speed", &self.reaction_speed)
    }

    /// `MOTION_COMPENSATION.vertical_actuators_stroke` now.
    fn stroke_now(&self) -> NativeSetting {
        self.runtime.value("MOTION_COMPENSATION", "vertical_actuators_stroke", &self.vertical_stroke_mm)
    }

    /// `PROSIMU.invert_tl` as a boolean now.
    fn invert_now(&self) -> Result<bool, String> {
        self.runtime.switch("PROSIMU", "invert_tl", &self.invert_tl)
    }
}

/// The PROSIMU driver in its four variants.
pub struct Prs200Driver {
    settings: LiveSettings<ProsimuSettings>,
    activity: LiveSettings<HostActivity>,
    board_ports: Box<dyn BoardPorts>,
    boards: SharedBoards,
    spawn_startup: StartupSpawner,
    /// The Dyadic actuator records, keyed by their position.
    dyadic: Vec<ActuatorRecord>,
    reaction: ReactionSpeed,
    /// The caller's list after the in-place edits of the last frame.
    edited: Option<Vec<i64>>,
    /// Whether the last frame wrote into the caller's list.
    wrote: bool,
    /// The model's controller count, which a connect lowers by one on every call while
    /// `enable_dyatic_tl` is off.
    controllers: BTreeMap<&'static str, i64>,
    assignment_ok: bool,
    /// The port of the Dyadic TL axis (default `''`).
    tl_port: LiveSettings<String>,
    travel: Option<CornerTravel>,
}

impl Prs200Driver {
    pub fn new(settings: LiveSettings<ProsimuSettings>, activity: LiveSettings<HostActivity>) -> Self {
        Self::with_board_ports(settings, activity, Box::new(SystemBoardPorts))
    }

    /// A driver opening its board ports through `boards` (tests).
    pub fn with_board_ports(
        settings: LiveSettings<ProsimuSettings>,
        activity: LiveSettings<HostActivity>,
        boards: Box<dyn BoardPorts>,
    ) -> Self {
        Prs200Driver {
            settings,
            activity,
            board_ports: boards,
            boards: SharedBoards::default(),
            spawn_startup: Box::new(spawn_startup_thread),
            dyadic: Vec::new(),
            reaction: ReactionSpeed::new(),
            edited: None,
            wrote: false,
            controllers: BTreeMap::new(),
            assignment_ok: false,
            tl_port: LiveSettings::default(),
            travel: None,
        }
    }

    /// The same driver starting its start-up threads through `spawner` (tests run them on a
    /// manual clock).
    pub fn with_startup_spawner(mut self, spawner: StartupSpawner) -> Self {
        self.spawn_startup = spawner;
        self
    }

    pub fn factory(settings: LiveSettings<ProsimuSettings>, activity: LiveSettings<HostActivity>) -> DriverFactory {
        Self::factory_with_tl_port(settings, activity, LiveSettings::default())
    }

    /// Every variant writes the same Dyadic TL port holder.
    pub fn with_tl_port_parameter(mut self, port: LiveSettings<String>) -> Self {
        self.tl_port = port;
        self
    }

    pub fn factory_with_tl_port(
        settings: LiveSettings<ProsimuSettings>,
        activity: LiveSettings<HostActivity>,
        port: LiveSettings<String>,
    ) -> DriverFactory {
        Box::new(move || {
            Box::new(Prs200Driver::new(settings.clone(), activity.clone()).with_tl_port_parameter(port.clone()))
        })
    }

    /// The axis assignment status.
    pub fn assignment_ok(&self) -> bool {
        self.assignment_ok
    }

    pub fn corner_travel(&self) -> Option<CornerTravel> {
        self.travel
    }

    /// The port of the Dyadic traction-loss axis found by the last TL scan, `""` when that scan found none
    /// or none ran yet (the Hardware page leaves this port out of the
    /// PRS200/VNM port lists).
    pub fn tl_port(&self) -> String {
        self.tl_port.get()
    }

    fn module(ctx: &MotionContext<'_>) -> Result<Prs200Module, MotionError> {
        Prs200Module::of(ctx.model.controller).ok_or(MotionError::NoDriver(ctx.model.controller.key()))
    }

    /// Disconnects: close both boards and drop the Dyadic axes.
    fn release(&mut self) {
        let mut boards = lock_boards(&self.boards);
        boards.a.close();
        boards.b.close();
        drop(boards);
        self.dyadic.clear();
    }

    /// Scan, power and home the Dyadic axes: the part of a connect that runs with more than one controller
    /// and an enabled Dyadic TL. `Ok(false)` is its early end: the scan left
    /// the port of record 0 empty, so 'something is wrong' is printed, 'Not connected' is stored
    /// with empty motion ports and the connect does not go on to the boards.
    ///
    /// An error ends the connect with what the steps before it stored: the records, and
    /// the Dyadic TL port once the scan itself completed.
    fn connect_dyadic(
        &mut self,
        module: Prs200Module,
        scan: ScanModule,
        slots: usize,
        ctx: &mut MotionContext<'_>,
    ) -> Result<bool, MotionError> {
        debug!("disconnect");
        self.release();
        // An empty record for key 0, or for every key below the controller count minus one
        // in the TL/surge variant.
        self.dyadic = (0..slots).map(|_| ActuatorRecord::default()).collect();
        scan_records(ctx.host, &mut self.dyadic, scan)?;
        let lines = module.connect_lines();
        let missing = |key: usize, line: u32| {
            move || MotionError::Native(format!("KeyError: {key}")).in_frame(module.connect_frame(line))
        };
        let tl_port = self.dyadic.first().ok_or_else(missing(0, lines.tl_port))?.com_port.clone();
        self.tl_port.set(tl_port.clone());
        if tl_port.is_empty() {
            python_print(&["something is wrong"]);
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
            return Ok(false);
        }
        let clock = ctx.host.clock();
        if module == Prs200Module::TlSurge {
            // Every record, found or not.
            for record in &mut self.dyadic {
                move_org(record, DYADIC_HOMING_MODE, clock, scan)?;
            }
            for (key, record) in self.dyadic.iter_mut().enumerate() {
                wait_for_homing(record, key, clock, scan);
            }
            python_print(&["homing completed"]);
            ctx.link.ports.insert(0, tl_port);
            // Motion port entry 1 is the port of record 1.
            let second = self.dyadic.get(1).ok_or_else(missing(1, lines.second_port))?;
            ctx.link.ports.insert(1, second.com_port.clone());
            python_print(&["dyadic set", &ports_repr(&ctx.link.ports)]);
        } else {
            python_print(&["homing "]);
            move_org(&mut self.dyadic[0], DYADIC_HOMING_MODE, clock, scan)?;
            wait_for_homing(&mut self.dyadic[0], 0, clock, scan);
            python_print(&["homing completed"]);
            ctx.link.ports.insert(0, tl_port);
            python_print(&["dialytic set", &ports_repr(&ctx.link.ports)]);
        }
        Ok(true)
    }

    /// Open the boards whose configured port is among the system's ports. A board that fails to open
    /// is only logged; the connection test follows, unhandled, on whatever the slot
    /// holds: the new board, the board of an earlier connection (still open, or closed by a disconnect), or
    /// no board, whose attribute error ends the connect.
    fn connect_boards(&mut self, module: Prs200Module, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let [key_a, key_b] = module.board_keys();
        let lines = module.connect_lines();
        // A port setting is a plain lookup: its key error occurs at the frame line of the read.
        let lookup = |line: u32| move |detail: String| MotionError::Native(detail).in_frame(module.connect_frame(line));
        if module == Prs200Module::Sv {
            // SV variant: both port settings are read and logged before the enumeration, so a
            // missing key is an error here even when no port is listed.
            let settings = self.settings.get();
            let port1 = settings.port1_now().map_err(lookup(lines.start_scan))?;
            let port2 = settings.port2_now().map_err(lookup(lines.start_scan))?;
            debug!("start scan of the prs200a: {} prs200b: {}", native_str(&port1), native_str(&port2));
        }
        let mut boards = lock_boards(&self.boards);
        let Boards { a, b } = &mut *boards;
        for port in ctx.host.serial_ports_checked()? {
            for (first, key, slot, letter) in [(true, key_a, &mut *a, "a"), (false, key_b, &mut *b, "b")] {
                let side = usize::from(!first);
                let configured = if first { self.settings.get().port1_now() } else { self.settings.get().port2_now() }
                    .map_err(lookup(lines.port[side]))?;
                if configured.as_str() != Some(port.path.as_str()) {
                    continue;
                }
                // The Dyadic, TL/surge and PRS50 variants print these steps; the SV variant logs them
                // at debug level with the board's letter in the text.
                let sv = module == Prs200Module::Sv;
                let details = ctx.host.port_details(&port);
                if sv {
                    debug!(
                        "Trying to connect PRS200{letter} to {} {} {}",
                        port.path,
                        details.description,
                        details.hwid_text()
                    );
                } else {
                    python_print(&[
                        "Trying to connect PRS200 to",
                        &port.path,
                        &details.description,
                        details.hwid_text(),
                    ]);
                }
                // The firmware version setting is read and converted to an integer
                // before the port opens.
                let version = self
                    .settings
                    .get()
                    .firmware_now()
                    .map_err(MotionError::Native)
                    .and_then(|raw| native::to_bigint(&raw));
                let board = match version {
                    Ok(version) => self
                        .board_ports
                        .open(&port.path)
                        .map(|link| Prs200Board::new_native(link, &version))
                        .map_err(|error| python::link_error_str(&error)),
                    Err(error) => Err(error.python_str()),
                };
                match board {
                    Ok(board) => *slot = BoardSlot::Open(board),
                    Err(detail) if sv => {
                        debug!("Failed to instance PRS200{letter} in this com port {detail}")
                    }
                    Err(detail) => python_print(&["Failed to instance PRS200 in this com port", &detail]),
                }
                match slot {
                    BoardSlot::Open(_) => {
                        if sv {
                            debug!("prs200{letter} is connected");
                        } else {
                            python_print(&[&format!("prs200 {letter} is connected")]);
                        }
                        ctx.link.ports.insert(key, port.path.clone());
                    }
                    BoardSlot::Closed if sv => debug!("disconnecting {letter}"),
                    BoardSlot::Closed => python_print(&["disconnecting"]),
                    BoardSlot::Empty => {
                        return Err(MotionError::Native(
                            "AttributeError: 'NoneType' object has no attribute 'is_connected'".into(),
                        )
                        .in_frame(module.connect_frame(lines.is_connected[side])))
                    }
                }
            }
        }
        Ok(())
    }

    /// The reaction-speed check of a frame.
    fn update_speed(&mut self, module: Prs200Module, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let a = self.activity.get().live();
        let state = game_state(&a.game_running, &a.listener_running, a.motion_test_mode, module.tests_listener());
        let now = ctx.host.clock().now();
        let style = module.reaction_style(ctx.model.name);
        let mut boards = lock_boards(&self.boards);
        let Boards { a: board_a, b: board_b } = &mut *boards;
        // Each read of the reaction speed setting fails at its own frame line.
        let lines = module.reaction_lines();
        let profile = usize::from(!matches!(style, Style::Sv { sv_model: true }));
        let mut reads = 0;
        let settings = &self.settings;
        self.reaction.check_with(
            style,
            &state,
            || {
                let line = lines.reads[profile][reads.min(1)];
                reads += 1;
                let frames = match lines.calc_call {
                    Some(call) => vec![module.at(CHECK_REACTION_SPEED, call), module.at("calc_reaction_speed", line)],
                    None => vec![module.at(CHECK_REACTION_SPEED, line)],
                };
                reaction_double(&settings.get().reaction_now(), frames)
            },
            now,
            |pair| send_reaction_pair(module, settings, pair, board_a, board_b),
        )
    }

    /// Starts the start-up thread of a connect. The worker reads its current model at entry and
    /// after the five initial sends; each board send reads the current packet layout.
    fn start_init_reaction_speed(&mut self, module: Prs200Module) {
        let (boards, settings, activity) = (Arc::clone(&self.boards), self.settings.clone(), self.activity.clone());
        (self.spawn_startup)(Box::new(move |clock: &mut dyn Clock| {
            let current = || {
                let a = activity.get().live();
                game_state(&a.game_running, &a.listener_running, a.motion_test_mode, module.tests_listener())
            };
            let error_line = std::cell::Cell::new(0);
            let current_style = std::cell::Cell::new(Style::Classic);
            let mut model_read = 0;
            let mut reaction_read = 0;
            if let Err(error) = init_reaction_speed_live(
                clock,
                current,
                || {
                    model_read += 1;
                    error_line.set(match module {
                        Prs200Module::Sv => {
                            if model_read == 1 {
                                402
                            } else {
                                426
                            }
                        }
                        Prs200Module::Prs50 => {
                            if model_read == 1 {
                                182
                            } else {
                                206
                            }
                        }
                        Prs200Module::Dyadic | Prs200Module::TlSurge => 0,
                    });
                    let style = module.reaction_style_native(&settings.get().model_now())?;
                    current_style.set(style);
                    Ok(style)
                },
                || {
                    reaction_read += 1;
                    error_line.set(init_reaction_line(module, current_style.get(), reaction_read));
                    reaction_double(&settings.get().reaction_now(), Vec::new())
                },
                |pair| send_startup_pair(&boards, module, &settings, pair),
            ) {
                use std::io::Write;
                let _ = write!(
                    std::io::stderr().lock(),
                    "Exception in thread init_reaction_speed:\nTraceback (most recent call last):\n{}",
                    init_reaction_failure(module, error_line.get(), &error)
                );
            }
        }));
    }

    /// Whether the Dyadic axes are in use for this frame (more than one controller and an enabled Dyadic TL),
    /// tested at `frame`.
    fn dyadic_active(
        &self,
        module: Prs200Module,
        model: &str,
        s: &ProsimuSettings,
        frame: PyFrame,
    ) -> Result<bool, MotionError> {
        Ok(module.has_dyadic() && self.controllers.get(model).is_some_and(|&n| n > 1) && s.dyadic_enabled(frame)?)
    }

    /// The traction-loss inversion: when the model allows it and `PROSIMU.invert_tl` is on, slot 2 becomes
    /// 65536 minus itself. It is the first step of the position pass (at `frame`), written into the caller's list.
    fn tl_adjusted(
        motors: &mut LegacyValue,
        s: &ProsimuSettings,
        model: &str,
        frame: PyFrame,
    ) -> Result<bool, MotionError> {
        if allow_change_tl_dir(model) && s.invert_now().map_err(|detail| boolean_setting_error(&detail, frame))? {
            let value = native::item(motors, TL_SLOT).map_err(|e| e.in_frame(frame))?;
            let value = native::sub(&native::int(FULL_SCALE as i64), &value).map_err(|e| e.in_frame(frame))?;
            native::assign(motors, TL_SLOT, value).map_err(|e| e.in_frame(frame))?;
            return Ok(true);
        }
        Ok(false)
    }

    /// The position pass of the Dyadic and SV variants (one TL axis) and of
    /// the TL/surge variant (TL and surge axes chosen by the axis assignment).
    fn send_dyadic(
        &mut self,
        module: Prs200Module,
        motors: &LegacyValue,
        s: &ProsimuSettings,
        ctx: &mut MotionContext<'_>,
    ) -> Result<(), MotionError> {
        let clock = ctx.host.clock();
        let lines = module.axis_lines();
        let file = module.file();
        let at = |line: u32| PyFrame::new(file, "send_actuator_pos", line);
        let mut slots = Vec::with_capacity(self.dyadic.len());
        for actuator in &mut self.dyadic {
            let assigned = module != Prs200Module::TlSurge
                || assigned_key(&s.axis_assignment, &AXIS.to_string(), &actuator.sn).is_some();
            let (speed, accel) = if assigned { (DYADIC_SPEED, DYADIC_ACCEL) } else { (-1, -1) };
            if actuator.speed == -1 {
                actuator.speed = speed;
                if actuator.speed == -1 {
                    self.assignment_ok = false;
                    return Ok(());
                }
            }
            if actuator.accel == -1 {
                actuator.accel = accel;
                if actuator.accel == -1 {
                    self.assignment_ok = false;
                    return Ok(());
                }
                let (speed, accel) = (actuator.speed, actuator.accel);
                python_print(&["setting velocity to", &speed.to_string(), &accel.to_string()]);
                write_velocity(
                    actuator,
                    speed,
                    accel,
                    clock,
                    PyFrame::new(file, "write_velocity", lines.write_velocity),
                )
                .map_err(|e| e.through(at(lines.write_first)))?;
            } else if speed != actuator.speed || accel != actuator.accel {
                python_print(&["setting velocity", &actuator.speed.to_string(), &actuator.accel.to_string()]);
                write_velocity(
                    actuator,
                    speed,
                    accel,
                    clock,
                    PyFrame::new(file, "write_velocity", lines.write_velocity),
                )
                .map_err(|e| e.through(at(lines.write_changed)))?;
            }
            if module == Prs200Module::TlSurge {
                // A key that is not an integer fails inside the id lookup, called from this frame line.
                let id = assigned_id(&s.axis_assignment, &AXIS.to_string(), &actuator.sn)
                    .map_err(|e| e.in_frames([at(lines.get_id), PyFrame::new(file, "get_id", TL_SURGE_GET_ID_INT)]))?;
                if native::eq(&id, &native::int(-1))? {
                    self.assignment_ok = false;
                    return Ok(());
                }
                let mut found = None;
                for &(option, slot) in TL_SURGE_SLOTS.iter() {
                    if native::eq(&native::int(option), &id).map_err(|e| e.in_frame(at(lines.mapping)))? {
                        found = Some(slot);
                        break;
                    }
                }
                let slot = found.ok_or_else(|| {
                    MotionError::Native(format!("KeyError: {}", native::str_of(&id))).in_frame(at(lines.mapping))
                })?;
                slots.push(slot);
            }
        }
        self.assignment_ok = true;
        if module != Prs200Module::TlSurge {
            slots = vec![TL_SLOT];
        }
        for (actuator, slot) in self.dyadic.iter_mut().zip(slots) {
            let statement = at(lines.position_map);
            let value = native::item(motors, slot).map_err(|e| e.in_frame(statement))?;
            // The slot rescaled from 0..65536 to 0..10000, negated and truncated.
            let mapped = native::map([
                &value,
                &native::int(0),
                &native::int(FULL_SCALE as i64),
                &native::int(0),
                &native::int(DYADIC_STROKE as i64),
            ])
            .map_err(|e| e.in_frame(statement))?;
            let position = native::trunc_float(-mapped)?;
            let link = actuator.link("move_abs_integer")?;
            let packed = native::pack_i32(&position, true)
                .map_err(|e| e.in_frame(crate::motion::scn6::tmbs::MOVE_ABS_INTEGER_PACK))?;
            link.move_absolute(AXIS, i32::from_be_bytes(packed));
        }
        Ok(())
    }

    /// The two board frames of a stream or test frame; an error of either is ignored on its own: each
    /// reads its four slots, then the board's position frame reduces each to 16 bits.
    fn send_boards(&mut self, module: Prs200Module, motors: &LegacyValue, model: &str, motor_count: usize) {
        let a_frame: [Option<usize>; 4] = match module {
            Prs200Module::Dyadic | Prs200Module::Sv if self.controllers.get(model) == Some(&1) && motor_count == 3 => {
                [Some(1), Some(0), Some(2), None]
            }
            _ => [Some(1), Some(0), Some(3), Some(4)],
        };
        let b_frame = match module {
            Prs200Module::Prs50 if model == SV_RK => [Some(5), Some(5), Some(2), Some(5)],
            Prs200Module::Prs50 => [Some(1), Some(0), Some(2), Some(4)],
            _ => [Some(1), Some(0), Some(3), Some(4)],
        };
        let mut boards = lock_boards(&self.boards);
        let Boards { a, b } = &mut *boards;
        for (board, frame) in [(a, a_frame), (b, b_frame)] {
            let Some(board) = board.open() else {
                continue;
            };
            let values: Result<Vec<LegacyValue>, MotionError> =
                frame.iter().map(|slot| slot.map_or(Ok(native::int(0)), |slot| native::item(motors, slot))).collect();
            if let Ok(values) = values {
                let _ = board.send_position_native(&values);
            }
        }
    }

    /// The actuator positions of a stream frame at `frame`: four entries, each
    /// its slot rescaled from 0..65535 to 0..the vertical stroke setting as an integer, for
    /// front-left `m[1]`, front-right `m[0]`, rear-left `m[3]` and rear-right `m[4]`, assigned once all exist.
    fn publish_travel(&mut self, motors: &LegacyValue, frame: PyFrame) -> Result<(), MotionError> {
        let mut corners = [0.0; 4];
        for (corner, slot) in corners.iter_mut().zip([1, 0, 3, 4]) {
            let value = native::item(motors, slot).map_err(|e| e.in_frame(frame))?;
            let stroke = self
                .settings
                .get()
                .stroke_now()
                .map_err(MotionError::Native)
                .and_then(|raw| native::to_int(&raw))
                .map_err(|e| e.in_frame(frame))?;
            *corner = native::map([&value, &native::int(0), &native::int(65_535), &native::int(0), &stroke])
                .map_err(|e| e.in_frame(frame))?;
        }
        let [front_left, front_right, rear_left, rear_right] = corners;
        self.travel = Some(CornerTravel { front_left, front_right, rear_left, rear_right });
        Ok(())
    }

    /// Common body of a stream frame and a test frame, on the caller's list.
    ///
    /// The Dyadic and SV variants run the position pass before they build
    /// the actuator positions; the TL/surge variant builds them first and runs
    /// the position pass after it. An error of either step leaves the other undone.
    fn frame(&mut self, motors: &mut LegacyValue, test: bool, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let module = Self::module(ctx)?;
        let model = ctx.model.name;
        let lines = module.frame_lines(test);
        let at = |line: u32| PyFrame::new(module.file(), lines.function, line);
        self.update_speed(module, ctx).map_err(|e| e.through(at(lines.check_reaction_speed)))?;
        let s = self.settings.get();
        if test {
            let a = self.activity.get().live();
            if !a.motion_test_mode && a.game_running == IDLE {
                // The idle overrides are made in the caller's list.
                for slot in [0, 1, 3, 4] {
                    native::assign(motors, slot, native::int(IDLE_TEST_POSITION))?;
                    self.wrote = true;
                }
                native::assign(motors, TL_SLOT, native::int(module.idle_test_tl()))?;
            }
        }
        let travel_first = module == Prs200Module::TlSurge;
        if !test && travel_first {
            self.publish_travel(motors, at(lines.stroke))?;
        }
        if self.dyadic_active(module, model, &s, at(lines.enable))? {
            // The position pass: the TL inversion is made in the caller's list.
            let axis = PyFrame::new(module.file(), "send_actuator_pos", module.axis_lines().invert);
            let sent = Self::tl_adjusted(motors, &s, model, axis).and_then(|_| {
                // The `&[i64]` callers take the list as edited from here on.
                self.wrote = true;
                self.send_dyadic(module, motors, &s, ctx)
            });
            sent.map_err(|e| e.through(at(lines.send_actuator_pos)))?;
        } else {
            self.wrote = true;
        }
        if !test && !travel_first {
            self.publish_travel(motors, at(lines.stroke))?;
        }
        self.send_boards(module, motors, model, ctx.model.motors);
        Ok(())
    }

    /// An `&[i64]` frame through the dynamic-value path; the list as the path left it is the edited
    /// frame.
    fn frame_compat(&mut self, motors: &[i64], test: bool, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.wrote = false;
        let (result, list) = native::compat(motors, |list| self.frame(list, test, ctx));
        self.edited = self.wrote.then(|| native::edited_vec(&list, motors));
        result
    }
}

/// The board sends of the reaction-speed check: board A (unless the variant first tests whether it exists), then
/// board B when it exists. A closed board is an error; errors are not handled.
fn send_reaction_pair(
    module: Prs200Module,
    settings: &LiveSettings<ProsimuSettings>,
    pair: Pair,
    board_a: &mut BoardSlot,
    board_b: &mut BoardSlot,
) -> Result<(), MotionError> {
    // The frame line of the check an error passes; a send to a board that was never opened
    // fails at that line itself. A write on a closed board carries the serial frame; a system
    // error of the port keeps no frames.
    let lines = module.reaction_lines();
    let left_at = |side: usize| {
        let frame = module.at(CHECK_REACTION_SPEED, lines.send[side]);
        move |e: MotionError| match e {
            MotionError::Native(detail) if detail == NO_BOARD => MotionError::Native(detail).in_frame(frame),
            other => other.through(frame),
        }
    };
    if !(module.guards_board_a() && matches!(board_a, BoardSlot::Empty)) {
        board_a.send_speed_current(module, settings, pair).map_err(left_at(0))?;
    }
    if !matches!(board_b, BoardSlot::Empty) {
        board_b.send_speed_current(module, settings, pair).map_err(left_at(1))?;
    }
    Ok(())
}

const CHECK_REACTION_SPEED: &str = "check_reaction_speed";

/// Writes an actuator's velocity: one TMBS velocity write, then remember the values.
///
/// `frame` is the variant's velocity write at the line of its protocol call: a reply
/// that is not hexadecimal is a value error through it.
fn write_velocity(
    actuator: &mut ActuatorRecord,
    speed: i64,
    accel: i64,
    clock: &mut dyn super::clock::Clock,
    frame: PyFrame,
) -> Result<(), MotionError> {
    actuator
        .link("write_velocity")
        .map_err(|e| e.in_frame(frame))?
        .action(AXIS, &Command::Velocity { speed, accel }, clock)
        .map_err(|e| super::scn6::axis::uncaught(e).through(frame))?;
    actuator.speed = speed;
    actuator.accel = accel;
    Ok(())
}

impl MotionDriver for Prs200Driver {
    /// Connects: optional Dyadic axes first, then the boards on the
    /// configured ports; connected when every controller of the (possibly lowered) count was found.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let module = Self::module(ctx)?;
        // The caller connects only while the motion switch is on and no motion error is set,
        // so the gate on those two values holds.
        debug!("starting");
        debug!("motion_switch is on and no error");
        let s = self.settings.get();
        let model = ctx.model.name;
        let lines = module.connect_lines();
        let disabled = module != Prs200Module::Prs50 && !s.dyadic_enabled(module.connect_frame(lines.enable[0]))?;
        let count = self.controllers.entry(model).or_insert(ctx.model.controllers as i64);
        if disabled {
            *count -= 1;
        }
        let count = *count;
        // Every variant logs the count, PRS50 included.
        debug!("number of controllers {count}");
        if let Some(scan) = module.scan_module() {
            if count > 1 && self.settings.get().dyadic_enabled(module.connect_frame(lines.enable[1]))? {
                let slots = match module {
                    Prs200Module::TlSurge => usize::try_from(count - 1).unwrap_or(0),
                    _ => 1,
                };
                if !self.connect_dyadic(module, scan, slots, ctx)? {
                    return Ok(());
                }
            }
        }
        self.connect_boards(module, ctx)?;
        if ctx.link.ports.len() as i64 != count {
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
            return Ok(());
        }
        ctx.link.status = LinkStatus::Connected;
        self.start_init_reaction_speed(module);
        Ok(())
    }

    /// Disconnects: close the boards, drop the Dyadic records.
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.release();
        Ok(())
    }

    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.frame_compat(motors, false, ctx)
    }

    /// A stream frame on the caller's list.
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.frame(motors, false, ctx)
    }

    /// A test frame on the caller's list.
    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.frame(motors, true, ctx)
    }

    /// A test frame: while idle and not in test mode the corner actuators go to 250 and the TL slot
    /// to 32768 (PRS50: 0).
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.frame_compat(motors, true, ctx)
    }

    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.travel.map(CornerTravel::to_positions)
    }

    /// The model's controller count as the last connect left it: lowered by one on every call while
    /// `enable_dyatic_tl` is off; the table value before the first call.
    fn actuators_travel(&self) -> Option<CornerTravel> {
        self.travel
    }

    fn take_edited_frame(&mut self) -> Option<Vec<i64>> {
        self.edited.take()
    }

    fn expected_controllers(&self, model: &ModelSpec) -> Option<i64> {
        self.controllers.get(model.name).copied()
    }

    /// The actuator records (the Dyadic TL axes), the axis assignment status and
    /// the Dyadic TL port as the Hardware page reads them.
    fn assignment(&self) -> Option<super::AxisAssignment> {
        Some(super::AxisAssignment {
            actuators: self
                .dyadic
                .iter()
                .enumerate()
                .map(|(key, record)| super::AssignableActuator {
                    key,
                    axis: record.axis.clone(),
                    com_port: record.com_port.clone(),
                    sn: record.sn.clone(),
                })
                .collect(),
            status: self.assignment_ok,
            tl_port: self.tl_port.get(),
        })
    }

    fn describe(&self) -> String {
        let boards = lock_boards(&self.boards);
        let ports: Vec<&str> = [&boards.a, &boards.b]
            .into_iter()
            .filter_map(|slot| match slot {
                BoardSlot::Open(board) => Some(board.port()),
                _ => None,
            })
            .chain(self.dyadic.iter().map(|record| record.com_port.as_str()).filter(|port| !port.is_empty()))
            .collect();
        format!("PROSIMU PRS200 on {}", ports.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::board::{position_frame, speed_frame};
    use super::*;
    use crate::motion::driver_support::testing::{ftdi_port, ScriptedLink, Written};
    use crate::motion::scn6::axis::testing::{scn6_device, with_velocity};
    use crate::motion::scn6::tmbs::{pack, Command};
    use crate::motion::testing::FakeHost;
    use crate::motion::{motion_model, LinkState};
    use crate::serial::{Link, LinkError};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn dyadic_scan_steps_use_stdout_in_the_prs200_connect_path() {
        const CHILD: &str = "OPENRIGCUE_PRS200_SCAN_OUTPUT";
        const STEPS: [&str; 7] =
            ["done", "seton is done", "set iterface", "set port no", "set default axis", "set sn", "set comport"];
        let fixture = |model: &str| match model {
            "T1000-3M PRS200" => Rig::new("T1000-3M PRS200", ports(P1, P2)),
            "T1000-6M PRS200 RK" => Rig::new("T1000-6M PRS200 RK", ports(P1, P2)),
            _ => panic!("unknown private fixture model"),
        };
        if let Ok(model) = std::env::var(CHILD) {
            let mut rig = fixture(&model);
            let messages = crate::motion::driver_support::testing::capture_debug_messages(|| rig.connect());
            assert!(!messages.iter().any(|message| STEPS.contains(&message.as_str()) || message == "done with"));
            return;
        }
        // Capture the real print destination in a separate test process; changing this process's
        // stdout would race unrelated driver tests. Every serial link remains the scripted Rig.
        for (model, matches) in [("T1000-3M PRS200", vec![TL]), ("T1000-6M PRS200 RK", vec![TL, SURGE, TL])] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "motion::prosimu::tests::dyadic_scan_steps_use_stdout_in_the_prs200_connect_path",
                    "--nocapture",
                ])
                .env(CHILD, model)
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let stdout = String::from_utf8(output.stdout).unwrap();
            // Everything the same connect prints, as this process's capture sees it: the child's stdout holds
            // those lines in that order between the test harness's own.
            let mut rig = fixture(model);
            let printed = mask_addresses(capture_diagnostics(|| rig.connect()));
            let printed: Vec<String> = printed
                .into_iter()
                .filter_map(|diagnostic| match diagnostic {
                    Diagnostic::Print(line) => Some(line),
                    Diagnostic::Log(_) | Diagnostic::Stderr(_) => None,
                })
                .collect();
            let child: Vec<String> = mask_addresses(stdout.lines().map(Diagnostic::print).collect())
                .into_iter()
                .filter_map(|diagnostic| match diagnostic {
                    Diagnostic::Print(line) => Some(line),
                    Diagnostic::Log(_) | Diagnostic::Stderr(_) => None,
                })
                .filter(|line| printed.contains(line))
                .collect();
            assert_eq!(child, printed, "{model} prints to the process's stdout: {stdout}");
            let steps: Vec<&str> = child
                .iter()
                .map(String::as_str)
                .take_while(|line| *line != "homing " && *line != "homing completed")
                .filter(|line| STEPS.contains(line) || line.starts_with("done with "))
                .collect();
            let expected: Vec<String> = matches
                .into_iter()
                .flat_map(|port| STEPS.into_iter().map(str::to_owned).chain([format!("done with {port}")]))
                .collect();
            assert_eq!(steps, expected, "{model} source prints these steps to stdout: {stdout}");
        }
    }

    #[test]
    fn native_prs200_connect_count_diagnostic_matches_the_three_dyadic_modules() {
        use crate::motion::driver_support::testing::capture_debug_messages;

        for (model, module) in [
            ("T1000-2M PRS200", Prs200Module::Dyadic),
            ("T1000-2M SV", Prs200Module::Sv),
            ("T1000-6M PRS200 RK", Prs200Module::TlSurge),
            ("T1000-6M SV RK", Prs200Module::Prs50),
        ] {
            let mut settings = ports("", "");
            settings.enable_dyadic_tl = Ok(false);
            let mut rig = Rig::new(model, settings);
            rig.driver.controllers.insert(model, 9);
            let messages = capture_debug_messages(|| {
                rig.call(|driver, context| {
                    assert_eq!(Prs200Driver::module(context).unwrap(), module);
                    driver.connect(context)
                })
                .unwrap();
            });
            // The first three log lines of each variant; PRS50 logs the
            // same three lines and never lowers the count.
            let count = if module == Prs200Module::Prs50 { 9 } else { 8 };
            let mut expected = vec![
                "starting".to_string(),
                "motion_switch is on and no error".into(),
                format!("number of controllers {count}"),
            ];
            if module == Prs200Module::Sv {
                // The SV variant's start-scan line, before the port enumeration; both fixture ports are ''.
                expected.push("start scan of the prs200a:  prs200b: ".into());
            }
            assert_eq!(messages, expected, "{module:?}");
            assert!(rig.startups.lock().unwrap().is_empty());
            assert!(rig.boards.written.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn native_prs200_sv_connect_diagnostics_keep_letters_port_details_and_catch_order() {
        use crate::discovery::ListPortInfo;
        use crate::motion::driver_support::testing::capture_debug_messages;

        let mut rig = Rig::new("T1000-2M SV", ports(P1, P2));
        for (port, description, hwid) in [(P1, "Board A", "HW A"), (P2, "Board B", "HW B")] {
            let mut details = ListPortInfo::new(port, "fake");
            details.description = description.into();
            details.hwid = Some(hwid.into());
            rig.host.listed_details.insert(port.into(), details);
        }
        let messages = capture_debug_messages(|| {
            rig.call(|driver, context| driver.connect_boards(Prs200Module::Sv, context)).unwrap();
        });
        assert_eq!(
            messages,
            [
                "start scan of the prs200a: /dev/ttyUSB5 prs200b: /dev/ttyUSB6",
                "Trying to connect PRS200a to /dev/ttyUSB5 Board A HW A",
                "prs200a is connected",
                "Trying to connect PRS200b to /dev/ttyUSB6 Board B HW B",
                "prs200b is connected",
            ]
        );
        rig.driver.release();
        rig.boards.fail_open.lock().unwrap().extend([P1.into(), P2.into()]);
        let messages = capture_debug_messages(|| {
            rig.call(|driver, context| driver.connect_boards(Prs200Module::Sv, context)).unwrap();
        });
        assert_eq!(
            messages,
            [
                "start scan of the prs200a: /dev/ttyUSB5 prs200b: /dev/ttyUSB6",
                "Trying to connect PRS200a to /dev/ttyUSB5 Board A HW A",
                "Failed to instance PRS200a in this com port busy",
                "disconnecting a",
                "Trying to connect PRS200b to /dev/ttyUSB6 Board B HW B",
                "Failed to instance PRS200b in this com port busy",
                "disconnecting b",
            ]
        );
        {
            let mut boards = lock_boards(&rig.driver.boards);
            boards.a = BoardSlot::Empty;
            boards.b = BoardSlot::Empty;
        }
        let messages = capture_debug_messages(|| {
            assert!(rig.call(|driver, context| driver.connect_boards(Prs200Module::Sv, context)).is_err());
        });
        assert_eq!(
            messages,
            [
                "start scan of the prs200a: /dev/ttyUSB5 prs200b: /dev/ttyUSB6",
                "Trying to connect PRS200a to /dev/ttyUSB5 Board A HW A",
                "Failed to instance PRS200a in this com port busy",
            ]
        );
        assert!(rig.startups.lock().unwrap().is_empty());
    }

    const P1: &str = "/dev/ttyUSB5";
    const P2: &str = "/dev/ttyUSB6";
    const TL: &str = "/dev/ttyUSB3";
    const SURGE: &str = "/dev/ttyUSB4";

    /// Board ports: every write recorded per path; `fail_open` / `fail_writes` name paths that fail.
    #[derive(Default, Clone)]
    struct Boards {
        written: Arc<Mutex<BTreeMap<String, Written>>>,
        fail_open: Arc<Mutex<Vec<String>>>,
        fail_writes: Arc<Mutex<Vec<String>>>,
    }

    impl BoardPorts for Boards {
        fn open(&mut self, path: &str) -> Result<Box<dyn Link>, LinkError> {
            if self.fail_open.lock().unwrap().iter().any(|p| p == path) {
                return Err(LinkError::Io(std::io::Error::other("busy")));
            }
            let mut link = ScriptedLink::silent(path);
            link.fail_writes = self.fail_writes.lock().unwrap().iter().any(|p| p == path);
            self.written.lock().unwrap().insert(path.into(), link.written.clone());
            Ok(Box::new(link))
        }
    }

    impl Boards {
        fn written(&self, path: &str) -> Vec<Vec<u8>> {
            self.written.lock().unwrap().get(path).map(|w| w.lock().unwrap().clone()).unwrap_or_default()
        }
    }

    struct Rig {
        driver: Prs200Driver,
        host: FakeHost,
        link: LinkState,
        boards: Boards,
        settings: LiveSettings<ProsimuSettings>,
        activity: LiveSettings<HostActivity>,
        model: &'static str,
        /// The start-up threads started so far, not yet run.
        startups: Arc<Mutex<Vec<StartupJob>>>,
    }

    impl Rig {
        fn new(model: &'static str, mut settings: ProsimuSettings) -> Self {
            let mut host = FakeHost::default();
            host.ports.push(ftdi_port(P1));
            host.ports.push(ftdi_port(P2));
            // One SCN6 per Dyadic slot of the model (the scan keeps the last match of a slot, see
            // `scan_with_one_slot_keeps_the_last_controller`).
            let scn6: &[(&str, u32)] = match model {
                "T1000-6M PRS200 RK" => &[(TL, 0x11), (SURGE, 0x22)],
                _ => &[(TL, 0x11)],
            };
            for &(path, sn) in scn6 {
                host.ports.push(ftdi_port(path));
                host = host.device(path, with_velocity(scn6_device(sn, 7), 5000, 80));
            }
            // Board ports answer nothing on TMBS: make them non-SCN6 for the Dyadic scan.
            let boards = Boards::default();
            settings.model = Ok(model.into());
            let settings = LiveSettings::new(settings);
            let activity = LiveSettings::new(HostActivity::default());
            let startups: Arc<Mutex<Vec<StartupJob>>> = Arc::default();
            let collected = Arc::clone(&startups);
            let driver = Prs200Driver::with_board_ports(settings.clone(), activity.clone(), Box::new(boards.clone()))
                .with_startup_spawner(Box::new(move |job| collected.lock().unwrap().push(job)));
            Rig { driver, host, link: LinkState::default(), boards, settings, activity, model, startups }
        }
        /// Runs the start-up threads started so far to their end, on the host's clock.
        fn run_startups(&mut self) -> usize {
            let jobs: Vec<StartupJob> = std::mem::take(&mut *self.startups.lock().unwrap());
            let count = jobs.len();
            for job in jobs {
                job(&mut self.host.clock.clone());
            }
            count
        }
        fn board_b_open(&self) -> bool {
            lock_boards(&self.driver.boards).b.is_open()
        }
        fn call<R>(&mut self, f: impl FnOnce(&mut Prs200Driver, &mut MotionContext<'_>) -> R) -> R {
            let spec = motion_model(self.model, false).unwrap();
            let mut ctx = MotionContext { model: spec, link: &mut self.link, host: &mut self.host };
            f(&mut self.driver, &mut ctx)
        }
        fn connect(&mut self) {
            self.link.ports.clear();
            self.call(|d, ctx| d.connect(ctx)).unwrap();
        }
        fn try_stream(&mut self, motors: &[i64]) -> Result<(), MotionError> {
            self.call(|d, ctx| d.send_stream(motors, ctx))
        }
        fn stream(&mut self, motors: &[i64]) {
            self.try_stream(motors).unwrap();
        }
        fn test_frame(&mut self, motors: &[i64]) {
            self.call(|d, ctx| d.send_test(motors, ctx)).unwrap();
        }
    }

    fn ports(port1: &str, port2: &str) -> ProsimuSettings {
        ProsimuSettings {
            port1: Ok(port1.into()),
            port2: Ok(port2.into()),
            vertical_stroke_mm: Ok(LegacyValue::Int(100)),
            ..ProsimuSettings::default()
        }
    }

    fn positions(frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
        frames.iter().filter(|f| f[1] == 0x02).cloned().collect()
    }

    #[test]
    fn live_motion_tl_port_is_published_before_a_later_board_failure() {
        let parameter = LiveSettings::new(String::new());
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.driver = rig.driver.with_tl_port_parameter(parameter.clone());
        rig.boards.fail_open.lock().unwrap().push(P1.into());
        assert!(rig.call(|driver, ctx| driver.connect(ctx)).is_err());
        assert_eq!(parameter.get(), TL);
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(TL));
        assert!(!rig.link.ports.contains_key(&1), "the failing board was never published");
    }

    #[test]
    fn live_motion_tl_port_is_shared_across_modules_and_survives_model_changes() {
        let parameter = LiveSettings::new(String::new());
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.driver = rig.driver.with_tl_port_parameter(parameter.clone());
        rig.connect();
        assert_eq!(parameter.get(), TL);
        rig.call(|driver, ctx| driver.disconnect(ctx)).unwrap();
        assert_eq!(parameter.get(), TL, "disconnect leaves the last published parameter alone");

        let mut other = Rig::new("T1000-2M PRS200", ports(P1, ""));
        other.driver = other.driver.with_tl_port_parameter(parameter.clone());
        other.connect();
        assert_eq!(parameter.get(), TL, "a model without a TL scan leaves the global value alone");
        assert_eq!(other.driver.assignment().unwrap().tl_port, TL);

        rig.host.devices.remove(TL);
        rig.connect();
        assert_eq!(parameter.get(), "", "an unsuccessful TL scan publishes its empty port");
        assert_eq!(other.driver.assignment().unwrap().tl_port, "");
    }

    #[test]
    fn single_board_model_connects_on_its_port() {
        // T1000-2M PRS200: one controller → no Dyadic; PRS200 on prs200_com_port1, motion port entry 1.
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&1).map(String::as_str), Some(P1));
        assert!(rig.host.log(TL).opens.is_empty());
    }

    #[test]
    fn configured_port_must_exist() {
        // The connect compares each listed port with prs200_com_port1.
        let mut rig = Rig::new("T1000-2M PRS200", ports("/dev/ttyUSB9", ""));
        rig.connect();
        assert!(!rig.link.is_connected());
        assert!(rig.link.ports.is_empty());
    }

    #[test]
    fn tl_model_finds_the_dyadic_axis_and_homes_it_with_mode_07() {
        // T1000-3M PRS200 (2 controllers = PRS200 + TL): motion ports {0: TL, 1: PRS200}.
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        assert_eq!(rig.driver.tl_port(), "");
        rig.connect();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(TL));
        // The Dyadic TL port is the port of record 0.
        assert_eq!(rig.driver.tl_port(), TL);
        assert!(rig.host.log(TL).written().contains(&pack(0, "o0700000000")));
    }

    #[test]
    fn tl_slot_keeps_the_last_scn6_in_comports_order() {
        // Dyadic variant: the port loop does not stop after 'done with', so with two SCN6 controllers the TL record ends
        // on the later port; the earlier one is opened and powered, then dropped (never homed).
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.host.ports.push(ftdi_port(SURGE));
        rig.host = std::mem::take(&mut rig.host).device(SURGE, scn6_device(0x22, 7));
        rig.connect();
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some(SURGE));
        assert_eq!(rig.driver.tl_port(), SURGE);
        assert!(rig.host.log(SURGE).written().contains(&pack(0, "o0700000000")));
        assert!(!rig.host.log(TL).written().contains(&pack(0, "o0700000000")));
    }

    #[test]
    fn tl_port_is_cleared_when_the_scan_finds_nothing() {
        // The port of record 0 ('' without a match) is stored before it is tested.
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        assert_eq!(rig.driver.tl_port(), TL);
        rig.host.devices.remove(TL);
        rig.connect();
        assert!(!rig.link.is_connected());
        assert_eq!(rig.driver.tl_port(), "");
    }

    #[test]
    fn disabled_tl_lowers_the_controller_count_on_every_connect() {
        // enable_dyatic_tl = no: the controller count drops by one on each call.
        let mut s = ports(P1, "");
        s.enable_dyadic_tl = Ok(false);
        let mut rig = Rig::new("T1000-5M PRS200 (Upgrade)", s);
        rig.connect();
        assert_eq!(MotionDriver::expected_controllers(&rig.driver, motion_model(rig.model, false).unwrap()), Some(2));
        assert!(!rig.link.is_connected());
        rig.connect();
        assert_eq!(MotionDriver::expected_controllers(&rig.driver, motion_model(rig.model, false).unwrap()), Some(1));
        assert!(rig.link.is_connected(), "one board now completes the lowered count");
    }

    /// The connect pass compares the held ports with the controller count that the driver's connect
    /// lowered (3 → 2 → 1 with `enable_dyatic_tl = no`): once one board completes the lowered count, the registry's
    /// port check keeps the connection instead of demoting it against the table value 3.
    #[test]
    fn registry_keeps_a_connection_of_the_lowered_count() {
        use crate::motion::{ConnectPassInputs, DriverEnvironment, DriverKind, MotionRegistry};
        let mut s = ports(P1, "");
        s.enable_dyadic_tl = Ok(false);
        let mut rig = Rig::new("T1000-5M PRS200 (Upgrade)", s);
        let spec = motion_model(rig.model, false).unwrap();
        assert_eq!(spec.controllers, 3);
        let (settings, activity, boards) = (rig.settings.clone(), rig.activity.clone(), rig.boards.clone());
        let mut registry =
            MotionRegistry::new(DriverEnvironment { windows_drivers: true, ..DriverEnvironment::srs_linux() });
        registry.register(
            DriverKind::Prs200Dyadic,
            Box::new(move || {
                Box::new(Prs200Driver::with_board_ports(settings.clone(), activity.clone(), Box::new(boards.clone())))
            }),
        );
        let inputs =
            ConnectPassInputs { model: rig.model, motion_switch_on: true, auto_search: true, enable_debug: false };
        registry.connect_pass(&inputs, &mut rig.host);
        assert!(!registry.link().is_connected(), "count 2, one board found");
        registry.connect_pass(&inputs, &mut rig.host);
        assert!(registry.link().is_connected(), "count 1 completed and kept by the port check");
        registry.connect_pass(&inputs, &mut rig.host);
        assert!(registry.link().is_connected());
    }

    #[test]
    fn stream_routes_slots_to_board_channels() {
        // A stream frame: board A gets (m1, m0, m3, m4); 3-motor single board: (m1, m0, m2, 0).
        let mut rig = Rig::new("T1000-5M PRS200 (Upgrade)", ports(P1, P2));
        rig.settings.update(|s| s.enable_dyadic_tl = Ok(false));
        rig.connect(); // 3 → 2: PRS200 + PRS50
        assert!(rig.link.is_connected());
        let m = [100, 200, 300, 400, 500, 600];
        rig.stream(&m);
        let a = positions(&rig.boards.written(P1));
        assert_eq!(a.last().unwrap(), &position_frame(3, [200, 100, 400, 500]));
        let b = positions(&rig.boards.written(P2));
        assert_eq!(b.last().unwrap(), &position_frame(3, [200, 100, 400, 500]));
        // The actuator positions: FL m1, FR m0, RL m3, RR m4.
        let t = rig.driver.corner_travel().unwrap();
        assert!((t.front_left - 200.0 * 100.0 / 65535.0).abs() < 1e-9);
        assert!((t.rear_right - 500.0 * 100.0 / 65535.0).abs() < 1e-9);

        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.settings.update(|s| s.enable_dyadic_tl = Ok(false));
        rig.connect(); // 2 → 1
        rig.stream(&m);
        let a = positions(&rig.boards.written(P1));
        assert_eq!(a.last().unwrap(), &position_frame(3, [200, 100, 300, 0]));
    }

    #[test]
    fn first_frame_sends_the_slow_speed_profile() {
        // The reaction-speed check on the first frame: target (4, 200) ≠ last sent (80, 80).
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        rig.stream(&[32768; 6]);
        let w = rig.boards.written(P1);
        assert_eq!(w[0], speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap());
    }

    #[test]
    fn native_prs200_startup_model_missing_at_entry_stops_before_the_first_packet() {
        let mut rig = Rig::new("T1000-2M SV", ports(P1, ""));
        rig.connect();
        rig.settings.update(|settings| settings.model = Err("KeyError: 'model'".into()));
        let before = rig.host.now();
        assert_eq!(rig.run_startups(), 1);
        assert_eq!(rig.host.now(), before);
        assert!(rig.boards.written(P1).is_empty());
    }

    #[test]
    fn native_prs200_startup_model_packet_layout_is_read_between_board_sends() {
        struct ChangeModel {
            settings: LiveSettings<ProsimuSettings>,
            written: Arc<Mutex<BTreeMap<String, Written>>>,
        }
        impl BoardPorts for ChangeModel {
            fn open(&mut self, path: &str) -> Result<Box<dyn Link>, LinkError> {
                let settings = self.settings.clone();
                let path = path.to_owned();
                let changes = path == P1;
                let link = ScriptedLink::new(&path, move |_| {
                    if changes {
                        settings.update(|settings| settings.model = Ok("T1000-2M PRS200".into()));
                    }
                    Vec::new()
                });
                self.written.lock().unwrap().insert(path, link.written.clone());
                Ok(Box::new(link))
            }
        }
        let mut rig = Rig::new("T1000-6M SV RK", ports(P1, P2));
        let written: Arc<Mutex<BTreeMap<String, Written>>> = Arc::default();
        rig.driver.board_ports = Box::new(ChangeModel { settings: rig.settings.clone(), written: written.clone() });
        rig.connect();
        rig.stream(&[32768; 6]);
        let written = written.lock().unwrap();
        assert_eq!(written[P1].lock().unwrap()[0], speed_frame(3, SpeedLayout::AccelSpeedPairs, 5, 5).unwrap());
        assert_eq!(written[P2].lock().unwrap()[0], speed_frame(3, SpeedLayout::Mode4, 5, 5).unwrap());
    }

    #[test]
    fn native_prs200_reaction_mock_startup_stops_before_a_sixth_packet() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.settings.update(|settings| settings.reaction_speed = Err("KeyError: 'reaction_speed'".into()));
        rig.activity.update(|activity| {
            activity.game_running = "game".into();
            activity.listener_running = "game".into();
        });
        rig.connect();
        assert_eq!(rig.run_startups(), 1);
        assert_eq!(rig.boards.written(P1), vec![speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap(); 5]);
    }

    #[test]
    fn native_prs200_ports_defer_missing_values_until_enumeration() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, P2));
        rig.settings.update(|settings| {
            settings.port1 = Err("KeyError: 'prs200_com_port1'".into());
            settings.port2 = Err("KeyError: 'prs200_com_port2'".into());
        });
        rig.host.ports.clear();
        rig.connect();
        assert!(rig.boards.written.lock().unwrap().is_empty());
        rig.host.ports.push(ftdi_port(P1));
        let error = rig.call(|driver, context| driver.connect(context)).unwrap_err();
        assert_eq!(error.python_str(), "'prs200_com_port1'");
        assert!(rig.boards.written.lock().unwrap().is_empty());
        assert!(rig.startups.lock().unwrap().is_empty());
    }

    #[test]
    fn native_prs200_ports_missing_second_read_retains_the_first_board() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, P2));
        rig.settings.update(|settings| settings.port2 = Err("KeyError: 'prs200_com_port2'".into()));
        let error = rig.call(|driver, context| driver.connect(context)).unwrap_err();
        assert_eq!(error.python_str(), "'prs200_com_port2'");
        assert!(lock_boards(&rig.driver.boards).a.is_open());
        assert_eq!(rig.link.ports.get(&1).map(String::as_str), Some(P1));
        assert!(rig.boards.written.lock().unwrap().contains_key(P1));
        assert!(!rig.boards.written.lock().unwrap().contains_key(P2));
        assert!(rig.startups.lock().unwrap().is_empty());
    }

    #[test]
    fn native_prs200_ports_read_a_second_port_change_after_the_first_open() {
        struct ChangePort {
            boards: Boards,
            settings: LiveSettings<ProsimuSettings>,
        }
        impl BoardPorts for ChangePort {
            fn open(&mut self, path: &str) -> Result<Box<dyn Link>, LinkError> {
                let link = self.boards.open(path)?;
                self.settings.update(|settings| {
                    settings.port2 = Ok(P1.into());
                    settings.firmware = Ok(LegacyValue::Int(-7));
                });
                Ok(link)
            }
        }
        let mut rig = Rig::new("T1000-6M SV RK", ports(P1, P2));
        rig.driver.board_ports = Box::new(ChangePort { boards: rig.boards.clone(), settings: rig.settings.clone() });
        rig.connect();
        assert_eq!(rig.link.ports.get(&2).map(String::as_str), Some(P1));
        assert!(!rig.boards.written.lock().unwrap().contains_key(P2));
        rig.stream(&[32768; 6]);
        assert_eq!(rig.boards.written(P1).last().unwrap(), &position_frame(-7, [32768; 4]));
    }

    #[test]
    fn native_prs200_ports_do_not_stringify_non_string_values() {
        for value in [
            LegacyValue::Bool(true),
            LegacyValue::Int(7),
            LegacyValue::Bytes(b"fixture".to_vec()),
            LegacyValue::List(Vec::new()),
            LegacyValue::None,
        ] {
            let mut rig = Rig::new("T1000-2M PRS200", ports(P1, P2));
            rig.settings.update(|settings| {
                settings.port1 = Ok(value.clone());
                settings.port2 = Ok(value.clone());
            });
            rig.host.ports = ["True", "7", "fixture", "[]", "None"].map(ftdi_port).to_vec();
            rig.connect();
            assert!(rig.boards.written.lock().unwrap().is_empty(), "{value:?}");
        }
    }

    #[test]
    fn native_prs200_stroke_error_retains_travel_and_test_frames_skip_the_read() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        rig.stream(&[32768; 6]);
        let previous = rig.driver.corner_travel();
        let writes = rig.boards.written(P1).len();
        rig.settings.update(|settings| {
            settings.vertical_stroke_mm = Err("ValueError: invalid literal for int() with base 10: 'bad'".into())
        });
        let error = rig.try_stream(&[1; 6]).unwrap_err();
        assert_eq!(error.python_str(), "invalid literal for int() with base 10: 'bad'");
        assert_eq!(rig.driver.corner_travel(), previous);
        assert_eq!(rig.boards.written(P1).len(), writes);
        rig.test_frame(&[1; 6]);
        assert_eq!(rig.driver.corner_travel(), previous);
        assert!(rig.boards.written(P1).len() > writes);
    }

    #[test]
    fn native_prs200_stroke_error_keeps_completed_dyadic_writes() {
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        rig.settings
            .update(|settings| settings.vertical_stroke_mm = Err("KeyError: 'vertical_actuators_stroke'".into()));
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(error.python_str(), "'vertical_actuators_stroke'");
        assert_eq!(rig.host.log(TL).written().last().unwrap(), &pack(0, &Command::MoveAbsolute(-5000).text()));
        assert_eq!(rig.driver.dyadic[0].speed, 5000);
        assert_eq!(rig.driver.dyadic[0].accel, 80);
        assert!(rig.driver.corner_travel().is_none());
        assert_eq!(rig.boards.written(P1), [speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap()]);
    }

    #[test]
    fn native_prs200_stroke_reads_changes_after_dyadic_io() {
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        let settings = rig.settings.clone();
        let velocity = pack(0, &Command::Velocity { speed: 5000, accel: 80 }.text());
        let link = ScriptedLink::new(TL, move |bytes| {
            if bytes == velocity {
                settings
                    .update(|settings| settings.vertical_stroke_mm = Err("ValueError: changed stroke fixture".into()));
                crate::motion::tmbs::testing::reply('0', "V10B000000")
            } else {
                Vec::new()
            }
        });
        let written = link.written.clone();
        rig.driver.dyadic[0].interface = Some(super::super::scn6::tmbs::TmbsLink::new(Box::new(link)));
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(error.python_str(), "changed stroke fixture");
        assert_eq!(written.lock().unwrap().last().unwrap(), &pack(0, &Command::MoveAbsolute(-5000).text()));
        assert!(rig.driver.corner_travel().is_none());
        assert_eq!(rig.boards.written(P1), [speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap()]);
    }

    #[test]
    fn native_prs200_firmware_error_is_deferred_until_a_matching_port() {
        let mut rig = Rig::new("T1000-2M PRS200", ports("", ""));
        rig.settings.update(|settings| settings.firmware = Err("KeyError: 'prs200_firmware'".into()));
        rig.connect();
        assert!(rig.boards.written.lock().unwrap().is_empty());
        assert!(rig.startups.lock().unwrap().is_empty());
    }

    #[test]
    fn native_prs200_enable_error_precedes_model_count_and_board_changes() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.settings.update(|settings| settings.enable_dyadic_tl = Err("KeyError: 'enable_dyatic_tl'".into()));
        let error = rig.call(|driver, context| driver.connect(context)).unwrap_err();
        assert_eq!(error.python_str(), "'enable_dyatic_tl'");
        assert!(rig.driver.controllers.is_empty());
        assert!(rig.boards.written.lock().unwrap().is_empty());
        assert!(rig.startups.lock().unwrap().is_empty());
    }

    #[test]
    fn native_prs200_enable_frame_failure_keeps_the_completed_speed_prefix() {
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        let tl_writes = rig.host.log(TL).written().len();
        rig.settings
            .update(|settings| settings.enable_dyadic_tl = Err("Exception: Boolean value expected. Got maybe".into()));
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(error.python_str(), "Boolean value expected. Got maybe");
        assert_eq!(rig.boards.written(P1), [speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap()]);
        assert_eq!(rig.host.log(TL).written().len(), tl_writes);
        assert!(rig.driver.edited.is_none());
    }

    #[test]
    fn native_prs200_flags_are_not_read_by_prs50_or_a_single_board_frame() {
        for model in ["T1000-2M PRS200", "T1000-6M SV RK"] {
            let mut rig = Rig::new(model, ports(P1, P2));
            rig.connect();
            rig.settings.update(|settings| {
                settings.enable_dyadic_tl = Err("KeyError: 'enable_dyatic_tl'".into());
                settings.invert_tl = Err("KeyError: 'invert_tl'".into());
            });
            if model == "T1000-6M SV RK" {
                rig.connect();
            }
            rig.stream(&[32768; 6]);
            assert!(!positions(&rig.boards.written(P1)).is_empty());
        }
    }

    #[test]
    fn native_prs200_invert_frame_failure_keeps_the_completed_speed_prefix() {
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        let tl_writes = rig.host.log(TL).written().len();
        rig.settings.update(|settings| settings.invert_tl = Err("Exception: Boolean value expected. Got maybe".into()));
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(error.python_str(), "Boolean value expected. Got maybe");
        assert_eq!(rig.boards.written(P1), [speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap()]);
        assert_eq!(rig.host.log(TL).written().len(), tl_writes);
        assert!(rig.driver.edited.is_none());
        let mut list = native::list_of(&[32768; 6]);
        let wrote = Prs200Driver::tl_adjusted(
            &mut list,
            &rig.settings.get(),
            "T1000-2M PRS200",
            crate::motion::driver_support::STR2BOOL,
        )
        .unwrap();
        assert!(!wrote);
        assert_eq!(list, native::list_of(&[32768; 6]));
    }

    #[test]
    fn native_prs200_firmware_error_does_not_open_a_port_before_the_outer_failure() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.settings.update(|settings| settings.firmware = Err("KeyError: 'prs200_firmware'".into()));
        let error = rig.call(|driver, context| driver.connect(context)).unwrap_err();
        // Dyadic variant: the connection test of board A after a failed open left no board.
        assert_eq!(error.python_str(), "'NoneType' object has no attribute 'is_connected'");
        assert_eq!(
            error.python_traceback().unwrap(),
            "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
             backend.controllers.motion_helpers.connect\n  File \"backend/controllers/prs200_dyadic.py\", line 279, in \
             backend.controllers.prs200_dyadic.connect\nAttributeError: 'NoneType' object has no attribute \
             'is_connected'\n"
        );
        assert!(rig.boards.written.lock().unwrap().is_empty());
        assert!(rig.startups.lock().unwrap().is_empty());
    }

    #[test]
    fn connect_setting_failures_raise_in_their_statement_or_inside_str2bool() {
        // A missing key is an error at the frame line that reads it (the Dyadic and TL/surge variants;
        // the SV variant for the ports its start-scan line reads). A value that is not a boolean fails inside
        // the boolean conversion's frame, called from that line.
        let traceback = |model: &'static str, change: fn(&mut ProsimuSettings)| {
            let mut settings = ports(P1, "");
            change(&mut settings);
            let mut rig = Rig::new(model, settings);
            rig.call(|driver, context| driver.connect(context)).unwrap_err().python_traceback().unwrap()
        };
        let head =
            "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
                    backend.controllers.motion_helpers.connect\n";
        assert_eq!(
            traceback("T1000-3M PRS200", |s| s.enable_dyadic_tl = Err("KeyError: 'enable_dyatic_tl'".into())),
            format!(
                "{head}  File \"backend/controllers/prs200_dyadic.py\", line 113, in \
                 backend.controllers.prs200_dyadic.connect\nKeyError: 'enable_dyatic_tl'\n"
            )
        );
        assert_eq!(
            traceback("T1000-6M PRS200 RK", |s| {
                s.enable_dyadic_tl = Err("Exception: Boolean value expected. Got maybe".into())
            }),
            format!(
                "{head}  File \"backend/controllers/prs200_dyadic_tl_surge.py\", line 109, in \
                 backend.controllers.prs200_dyadic_tl_surge.connect\n  File \"backend/functions.py\", line 261, in \
                 backend.functions.str2bool\nException: Boolean value expected. Got maybe\n"
            )
        );
        assert_eq!(
            traceback("T1000-2M SV", |s| s.port2 = Err("KeyError: 'prs200_com_port2'".into())),
            format!(
                "{head}  File \"backend/controllers/prs200_dyadic_sv.py\", line 267, in \
                 backend.controllers.prs200_dyadic_sv.connect\nKeyError: 'prs200_com_port2'\n"
            )
        );
        assert_eq!(
            traceback("T1000-5M SV", |s| s.port1 = Err("KeyError: 'prs200_com_port1'".into())),
            format!(
                "{head}  File \"backend/controllers/prs200_prs50.py\", line 106, in \
                 backend.controllers.prs200_prs50.connect\nKeyError: 'prs200_com_port1'\n"
            )
        );
    }

    #[test]
    fn native_prs200_firmware_error_retains_the_previous_board_and_packet_format() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.settings.update(|settings| settings.firmware = Ok(LegacyValue::Int(-7)));
        rig.connect();
        rig.stream(&[32768; 6]);
        assert_eq!(rig.boards.written(P1)[0], speed_frame(-7, SpeedLayout::Mode4, 4, 200).unwrap());
        let previous = rig.boards.written.lock().unwrap()[P1].clone();
        let count = rig.boards.written(P1).len();
        rig.settings.update(|settings| {
            settings.firmware = Err("ValueError: invalid literal for int() with base 10: 'bad'".into())
        });
        rig.connect();
        assert!(rig.link.is_connected());
        assert!(Arc::ptr_eq(&previous, &rig.boards.written.lock().unwrap()[P1]));
        rig.stream(&[32768; 6]);
        let written = rig.boards.written(P1);
        assert!(written.len() > count);
        assert_eq!(written.last().unwrap(), &position_frame(-7, [32768; 4]));
    }

    #[test]
    fn tl_axis_gets_velocity_once_then_negative_scaled_positions() {
        // The position pass: one velocity write (5000, 80); the position is m2 rescaled from 0..65536 to 0..10000, negated.
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        rig.stream(&[0, 0, 32768, 0, 0, 0]);
        rig.stream(&[0, 0, 65535, 0, 0, 0]);
        let w = rig.host.log(TL).written();
        let velocity = pack(0, &Command::Velocity { speed: 5000, accel: 80 }.text());
        assert_eq!(w.iter().filter(|f| **f == velocity).count(), 1);
        assert!(w.contains(&pack(0, &Command::MoveAbsolute(-5000).text())));
        assert_eq!(w.last().unwrap(), &pack(0, &Command::MoveAbsolute(-9999).text()));
    }

    #[test]
    fn invert_tl_uses_65536_minus_value() {
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.settings.update(|s| s.invert_tl = Ok(true));
        rig.connect();
        rig.stream(&[0, 0, 16384, 0, 0, 0]);
        let w = rig.host.log(TL).written();
        // 65536 - 16384 = 49152 → rescaled 7500 → -7500
        assert_eq!(w.last().unwrap(), &pack(0, &Command::MoveAbsolute(-7500).text()));
    }

    #[test]
    fn send_test_lowers_corners_while_idle() {
        // A test frame outside test mode with no game running → m0, m1, m3, m4 = 250.
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        rig.test_frame(&[1, 2, 3, 4, 5, 6]);
        let a = positions(&rig.boards.written(P1));
        assert_eq!(a.last().unwrap(), &position_frame(3, [250, 250, 250, 250]));
        rig.activity.update(|a| a.motion_test_mode = true);
        rig.test_frame(&[1, 2, 3, 4, 5, 6]);
        let a = positions(&rig.boards.written(P1));
        assert_eq!(a.last().unwrap(), &position_frame(3, [2, 1, 4, 5]));
    }

    #[test]
    fn prs50_second_board_routing() {
        // PRS50 variant: B gets (m1, m0, m2, m4), or (m5, m5, m2, m5) for T1000-6M SV RK.
        let m = [10, 20, 30, 40, 50, 60];
        let mut rig = Rig::new("T1000-5M SV", ports(P1, P2));
        rig.connect();
        assert!(rig.link.is_connected());
        rig.stream(&m);
        assert_eq!(positions(&rig.boards.written(P2)).last().unwrap(), &position_frame(3, [20, 10, 30, 50]));
        let mut rig = Rig::new("T1000-6M SV RK", ports(P1, P2));
        rig.connect();
        rig.stream(&m);
        assert_eq!(positions(&rig.boards.written(P2)).last().unwrap(), &position_frame(3, [60, 60, 30, 60]));
        // SV model speed layout: [acc, speed, …] with the SV idle profile 5/5 after a state change.
        let w = rig.boards.written(P1);
        assert_eq!(w[0], speed_frame(3, SpeedLayout::AccelSpeedPairs, 5, 5).unwrap());
    }

    #[test]
    fn idle_test_frame_centres_the_tl_slot() {
        // A test frame of the Dyadic, SV and TL/surge variants: m0, m1, m3, m4 = 250 and m2 = 32768 while idle. The
        // 3-motor single-board frame carries m2 on channel 3.
        for (model, tl_axis) in [("T1000-3M PRS200", false), ("T1000-3M SV", true)] {
            let mut rig = Rig::new(model, ports(P1, ""));
            rig.settings.update(|s| s.enable_dyadic_tl = Ok(tl_axis));
            rig.connect();
            rig.test_frame(&[1, 2, 3, 4, 5, 6]);
            assert_eq!(
                positions(&rig.boards.written(P1)).last().unwrap(),
                &position_frame(3, [250, 250, 32768, 0]),
                "{model}"
            );
        }
        // Dyadic variant with its TL axis: the centred slot reaches the Dyadic axis (32768 → -5000).
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        rig.test_frame(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(rig.host.log(TL).written().last().unwrap(), &pack(0, &Command::MoveAbsolute(-5000).text()));
        // TL/surge variant: the TL axis (option 0 → slot 2) gets the centred value.
        let mut s = ports(P1, "");
        s.axis_assignment.insert("0".into(), "0,00000011".into());
        s.axis_assignment.insert("1".into(), "0,00000022".into());
        let mut rig = Rig::new("T1000-6M PRS200 RK", s);
        rig.connect();
        rig.test_frame(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(rig.host.log(TL).written().last().unwrap(), &pack(0, &Command::MoveAbsolute(-5000).text()));
    }

    #[test]
    fn failed_first_board_open_raises_out_of_connect() {
        // Connect: a board A that fails to open only logs 'Failed to instance PRS200 in this com port'; the
        // connection test after it runs on an empty slot and its
        // attribute error ends the connect before board B and the count check.
        let mut rig = Rig::new("T1000-5M SV", ports(P1, P2));
        rig.boards.fail_open.lock().unwrap().push(P1.into());
        rig.link.ports.clear();
        assert!(rig.call(|d, ctx| d.connect(ctx)).is_err());
        assert!(!rig.link.is_connected());
        assert!(rig.link.ports.is_empty());
        assert!(!rig.board_b_open(), "board B is not reached");
    }

    #[test]
    fn failed_reopen_uses_the_board_the_global_still_holds() {
        // An open board of an earlier connection stays in slot A when the new open fails (the port is
        // still held): it tests as connected and the port is listed again.
        let mut rig = Rig::new("T1000-5M SV", ports(P1, P2));
        rig.connect();
        assert!(rig.link.is_connected());
        rig.boards.fail_open.lock().unwrap().push(P1.into());
        rig.link.status = LinkStatus::NotConnected;
        rig.connect();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.values().filter(|p| *p == P1).count(), 1);
        // After a disconnect the slot holds a closed board: 'disconnecting', and the count check decides.
        rig.call(|d, ctx| d.disconnect(ctx)).unwrap();
        rig.link.status = LinkStatus::NotConnected;
        rig.connect();
        assert!(!rig.link.is_connected());
        assert!(rig.link.ports.is_empty());
        assert!(rig.board_b_open(), "board B is still tried");
    }

    #[test]
    fn closed_board_b_raises_in_check_reaction_speed() {
        // The existence test of board B passes for a closed board, whose write is the port-not-open error.
        let mut rig = Rig::new("T1000-5M SV", ports(P1, P2));
        rig.connect();
        lock_boards(&rig.driver.boards).b.close();
        rig.activity.update(|a| a.game_running = "game".into());
        let error = rig.try_stream(&[32768; 12]).unwrap_err();
        assert!(error.to_string().contains("not open"), "{error}");
    }

    #[test]
    fn reaction_send_without_board_a_raises_except_in_sv() {
        // The reaction-speed check: the Dyadic, TL/surge and PRS50 variants send to board A
        // unguarded (an attribute error without a board); the SV variant tests whether board A exists.
        let mut rig = Rig::new("T1000-2M PRS200", ports("", ""));
        rig.connect();
        assert!(rig.try_stream(&[32768; 6]).is_err());
        // The pair is recorded as sent: the next frame has nothing to send and goes through.
        rig.stream(&[32768; 6]);
        let mut rig = Rig::new("T1000-5M SV", ports("", P2));
        rig.connect();
        assert!(rig.try_stream(&[32768; 6]).is_err());
        let mut rig = Rig::new("T1000-2M SV", ports("", ""));
        rig.connect();
        rig.stream(&[32768; 6]);
    }

    /// `(function, line)` of the frames recorded for an error, outermost first.
    fn trace(error: &MotionError) -> Vec<(&'static str, u32)> {
        error.frames().iter().map(|frame| (frame.function, frame.line)).collect()
    }

    /// The reaction-speed check runs first in a stream or test frame and none of its errors is handled:
    /// the board sends, the reaction speed reads
    /// (in the check itself, or in the target calculation for the SV-style variants).
    #[test]
    fn check_reaction_speed_errors_carry_the_module_lines() {
        use crate::motion::clock::Clock;
        let attribute_error =
            Some("AttributeError: 'NoneType' object has no attribute 'send_speed_and_acceleration'".to_string());
        let mut rig = Rig::new("T1000-2M PRS200", ports("", ""));
        rig.connect();
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 487), ("check_reaction_speed", 477)]);
        assert_eq!(error.exception_line(), attribute_error);
        let mut rig = Rig::new("T1000-5M SV", ports("", P2));
        rig.connect();
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 302), ("check_reaction_speed", 292)]);
        assert_eq!(error.exception_line(), attribute_error);
        let error = rig.call(|d, ctx| d.send_test(&[32768; 6], ctx)).unwrap_err();
        assert_eq!(trace(&error), [("send_test", 340), ("check_reaction_speed", 292)]);

        // A closed board is the port-not-open error in the serial write's frame, under the board's own write.
        let mut rig = Rig::new("T1000-5M SV", ports(P1, P2));
        rig.connect();
        lock_boards(&rig.driver.boards).b.close();
        rig.activity.update(|a| a.game_running = "game".into());
        let error = rig.try_stream(&[32768; 12]).unwrap_err();
        assert_eq!(
            trace(&error),
            [
                ("send_stream", 302),
                ("check_reaction_speed", 294),
                ("PRS200.send_speed_and_acceleration", 74),
                ("write", 306)
            ]
        );
        assert_eq!(
            error.exception_line().as_deref(),
            Some("serial.serialutil.PortNotOpenError: Attempting to use a port that is not open")
        );
        assert_eq!(error.python_str(), "Attempting to use a port that is not open");

        // The classic variant reads `reaction_speed` once the game has run for three seconds.
        let missing = || Err("KeyError: 'reaction_speed'".to_string());
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        rig.activity.update(|a| {
            a.game_running = "game".into();
            a.listener_running = "game".into();
        });
        rig.stream(&[32768; 6]);
        rig.host.clock.sleep(Duration::from_secs(4));
        rig.settings.update(|s| s.reaction_speed = missing());
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 487), ("check_reaction_speed", 461)]);
        assert_eq!(error.exception_line().as_deref(), Some("KeyError: 'reaction_speed'"));

        // The SV variant follows its target calculation on every frame after the three seconds.
        let mut rig = Rig::new("T1000-2M SV", ports(P1, ""));
        rig.connect();
        rig.activity.update(|a| {
            a.game_running = "game".into();
            a.listener_running = "game".into();
        });
        rig.stream(&[32768; 6]);
        rig.host.clock.sleep(Duration::from_secs(4));
        rig.stream(&[32768; 6]);
        rig.settings.update(|s| s.reaction_speed = missing());
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 523), ("check_reaction_speed", 501), ("calc_reaction_speed", 460)]);
    }

    #[test]
    fn startup_sends_reach_board_b_when_board_a_fails() {
        // The start-up sends: the errors of the board A and board B sends are handled separately.
        let mut rig = Rig::new("T1000-5M SV", ports(P1, P2));
        rig.boards.fail_writes.lock().unwrap().push(P1.into());
        rig.connect();
        assert!(rig.link.is_connected());
        let startup_pair = speed_frame(3, SpeedLayout::AccelSpeedPairs, 5, 5).unwrap();
        let sent_to_b = |rig: &Rig| rig.boards.written(P2).iter().filter(|f| **f == startup_pair).count();
        // The reaction-speed check fails on board A (not handled) before board B.
        assert!(rig.try_stream(&[32768; 6]).is_err());
        assert_eq!(sent_to_b(&rig), 0);
        // The previous game state was not recorded, so the next frame handles the state change again (the SV
        // reset makes the pair differ) and fails again.
        assert!(rig.try_stream(&[32768; 6]).is_err());
        assert_eq!(sent_to_b(&rig), 0);
        // The thread's five sends each reach board B although board A fails every time.
        assert_eq!(rig.run_startups(), 1);
        assert_eq!(sent_to_b(&rig), 5);
    }

    #[test]
    fn prs50_test_frame_also_zeroes_the_tl_slot() {
        let mut rig = Rig::new("T1000-5M SV", ports(P1, P2));
        rig.connect();
        rig.test_frame(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(positions(&rig.boards.written(P2)).last().unwrap(), &position_frame(3, [250, 250, 0, 250]));
    }

    #[test]
    fn tl_surge_maps_assignment_options_to_slots() {
        // TL/surge variant: assignment option → slot {0: 2, 1: 5}; motion port entries {0, 1} for the
        // Dyadic axes and {2, 3} for the boards.
        let mut s = ports(P1, "");
        s.axis_assignment.insert("0".into(), "0,00000011".into());
        s.axis_assignment.insert("1".into(), "0,00000022".into());
        let mut rig = Rig::new("T1000-6M PRS200 RK", s);
        rig.connect();
        assert!(rig.link.is_connected(), "{:?}", rig.link);
        assert_eq!(rig.link.ports.get(&2).map(String::as_str), Some(P1));
        rig.stream(&[0, 0, 32768, 0, 0, 65536]);
        assert!(rig.driver.assignment_ok());
        // serial 11 = option 0 (TL) → slot 2 → -5000; serial 22 = option 1 (surge) → slot 5 → -10000.
        assert_eq!(rig.host.log(TL).written().last().unwrap(), &pack(0, &Command::MoveAbsolute(-5000).text()));
        assert_eq!(rig.host.log(SURGE).written().last().unwrap(), &pack(0, &Command::MoveAbsolute(-10000).text()));
    }

    #[test]
    fn frame_path_exceptions_carry_their_compiled_frames() {
        // The steps of a stream or test frame that can fail, with the frames below them:
        // the Dyadic variant's stream frame (the enable test) → the position pass (invert_tl, possibly
        // inside the boolean conversion) and → the velocity write → the protocol (the stroke); a test frame;
        // the TL/surge variant's stream frame → the position pass → the id lookup.
        let frames = |error: &MotionError| -> Vec<String> {
            error.frames().iter().map(|frame| format!("{}:{}:{}", frame.file, frame.function, frame.line)).collect()
        };
        let classic = "backend/controllers/prs200_dyadic.py";
        let failing = |change: fn(&mut ProsimuSettings), test: bool| {
            let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
            rig.connect();
            rig.settings.update(change);
            let motors = [32768; 6];
            rig.call(|d, ctx| if test { d.send_test(&motors, ctx) } else { d.send_stream(&motors, ctx) }).unwrap_err()
        };
        let error = failing(|s| s.enable_dyadic_tl = Err("KeyError: 'enable_dyatic_tl'".into()), false);
        assert_eq!(frames(&error), [format!("{classic}:send_stream:489")]);
        let error = failing(|s| s.enable_dyadic_tl = Err("KeyError: 'enable_dyatic_tl'".into()), true);
        assert_eq!(frames(&error), [format!("{classic}:send_test:535")]);
        let error = failing(|s| s.invert_tl = Err("Exception: Boolean value expected. Got maybe".into()), false);
        assert_eq!(
            frames(&error),
            [
                format!("{classic}:send_stream:490"),
                format!("{classic}:send_actuator_pos:341"),
                "backend/functions.py:str2bool:261".to_string(),
            ]
        );
        let error = failing(|s| s.invert_tl = Err("KeyError: 'invert_tl'".into()), true);
        assert_eq!(frames(&error), [format!("{classic}:send_test:536"), format!("{classic}:send_actuator_pos:341")]);
        let error = failing(
            |s| s.vertical_stroke_mm = Err("ValueError: invalid literal for int() with base 10: 'x'".into()),
            false,
        );
        assert_eq!(frames(&error), [format!("{classic}:send_stream:492")]);

        // The first velocity write of the TL axis answers with a reply that is not hexadecimal.
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        let velocity = request(&Command::Velocity { speed: 5000, accel: 80 });
        let mut device = with_velocity(scn6_device(0x11, 7), 5000, 80);
        for (command, answer) in &mut device.replies {
            if *command == velocity {
                *answer = reply('0', "V10B00ZZ00");
            }
        }
        rig.host = std::mem::take(&mut rig.host).device(TL, device);
        rig.connect();
        let error = rig.try_stream(&[32768; 6]).unwrap_err();
        assert_eq!(error.to_string(), "ValueError: invalid literal for int() with base 16: b'ZZ'");
        assert_eq!(
            frames(&error),
            [
                format!("{classic}:send_stream:490"),
                format!("{classic}:send_actuator_pos:364"),
                format!("{classic}:write_velocity:96"),
                "backend/controllers/pytbmscom.py:TMBSCOMProtocol.write_velocity:673".to_string(),
                "backend/controllers/pytbmscom.py:TMBSCOMProtocol.parseADirectResponseFormat:221".to_string(),
            ]
        );

        let mut s = ports(P1, "");
        // Record 0 of this fixture is the controller with serial 22.
        s.axis_assignment.insert("tl".into(), "0,00000022".into());
        let mut rig = Rig::new("T1000-6M PRS200 RK", s);
        rig.connect();
        let error = rig.try_stream(&[0, 0, 32768, 0, 0, 65536]).unwrap_err();
        assert_eq!(error.to_string(), "ValueError: invalid literal for int() with base 10: 'tl'");
        let surge = "backend/controllers/prs200_dyadic_tl_surge.py";
        assert_eq!(
            frames(&error),
            [
                format!("{surge}:send_stream:553"),
                format!("{surge}:send_actuator_pos:422"),
                format!("{surge}:get_id:339")
            ]
        );
    }

    #[test]
    fn tl_surge_stroke_read_precedes_the_dyadic_axes() {
        // TL/surge variant's stream frame: the reaction-speed check, the actuator positions with their four
        // vertical_actuators_stroke reads, and only then the position pass (with more than one controller and
        // an enabled Dyadic TL). The Dyadic and SV variants run the position pass first.
        // A failing stroke read therefore stops this variant before any Dyadic write or list edit.
        let mut s = ports(P1, "");
        s.axis_assignment.insert("0".into(), "0,00000011".into());
        s.axis_assignment.insert("1".into(), "0,00000022".into());
        s.invert_tl = Ok(true);
        let mut rig = Rig::new("T1000-6M PRS200 RK", s);
        rig.connect();
        let written = |rig: &mut Rig| {
            let tl = rig.host.log(TL).written().len();
            (tl, rig.host.log(SURGE).written().len())
        };
        let before = written(&mut rig);
        rig.settings
            .update(|settings| settings.vertical_stroke_mm = Err("KeyError: 'vertical_actuators_stroke'".into()));
        let error = rig.try_stream(&[0, 0, 32768, 0, 0, 65536]).unwrap_err();
        assert_eq!(error.python_str(), "'vertical_actuators_stroke'");
        assert_eq!(written(&mut rig), before);
        assert_eq!(rig.driver.dyadic[0].speed, -1, "send_actuator_pos was not reached");
        assert!(rig.call(|driver, _| driver.take_edited_frame()).is_none(), "motor_pwm[2] was not inverted");
        // With the stroke readable the travel is published and the axes move as before.
        rig.settings.update(|settings| settings.vertical_stroke_mm = Ok(LegacyValue::Int(100)));
        rig.stream(&[0, 0, 32768, 0, 0, 65536]);
        assert!(rig.driver.corner_travel().is_some());
        assert_eq!(rig.host.log(SURGE).written().last().unwrap(), &pack(0, &Command::MoveAbsolute(-10000).text()));
    }

    #[test]
    fn startup_thread_sends_without_frames() {
        // A connect starts the start-up thread: five sends 2 s apart with no frame, then (idle) nothing.
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        let slow = speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap();
        let t0 = rig.host.now();
        assert_eq!(rig.run_startups(), 1);
        assert_eq!(rig.boards.written(P1).iter().filter(|f| **f == slow).count(), 5);
        assert_eq!(rig.boards.written(P1).len(), 5, "no in-game pair while idle");
        assert_eq!(rig.host.now() - t0, Duration::from_secs(10));
    }

    #[test]
    fn startup_thread_sends_the_game_pair_last() {
        // After the five sends: a game state other than 'none' → the in-game pair of reaction_speed, read then.
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        rig.activity.update(|a| {
            a.game_running = "dirt".into();
            a.listener_running = "dirt".into();
        });
        rig.settings.update(|s| s.reaction_speed = Ok(LegacyValue::Int(85)));
        rig.run_startups();
        let written = rig.boards.written(P1);
        assert_eq!(written.len(), 6);
        assert_eq!(written[5], speed_frame(3, SpeedLayout::Mode4, 202, 78).unwrap());
    }

    #[test]
    fn every_connect_starts_its_own_startup_thread() {
        // A reconnect starts a second start-up thread next to one that may still run: both sequences go
        // out, to the board slots as they are at each send.
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        rig.link.status = LinkStatus::NotConnected;
        rig.connect();
        let slow = speed_frame(3, SpeedLayout::Mode4, 4, 200).unwrap();
        assert_eq!(rig.run_startups(), 2);
        assert_eq!(rig.boards.written(P1).iter().filter(|f| **f == slow).count(), 10);
    }

    use crate::motion::driver_support::testing::{capture_diagnostics, capture_prints, Diagnostic};
    use crate::motion::scn6::tmbs::testing::{reply, request};
    use crate::motion::scn6::tmbs::Register;

    /// The printed form of an interface contains the object's address; the checks compare everything else.
    fn mask_addresses(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
        const MARK: &str = "TMBSCOMProtocol object at 0x";
        let mask = |mut text: String| {
            let mut from = 0;
            while let Some(found) = text[from..].find(MARK) {
                let start = from + found + MARK.len();
                let end = start + 16;
                assert!(text[start..end].bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)), "{text}");
                text.replace_range(start..end, "ADDR");
                from = start;
            }
            text
        };
        diagnostics
            .into_iter()
            .map(|diagnostic| match diagnostic {
                Diagnostic::Log(text) => Diagnostic::Log(mask(text)),
                Diagnostic::Print(text) => Diagnostic::Print(mask(text)),
                Diagnostic::Stderr(text) => Diagnostic::Stderr(mask(text)),
            })
            .collect()
    }

    /// The unpacking's print for a read that timed out (the response holds only its leading STX).
    const UNANSWERED: &str =
        "unpackMessage - message size less than expected, looks like its using the wrong baud rate 1 b'\\x02'";

    /// What one silent FTDI port gives at one baud rate in the PRS200 variants' scan.
    fn silent_attempt(port: &str, baud: u32) -> Vec<Diagnostic> {
        let name = port.rsplit('/').next().unwrap();
        vec![
            Diagnostic::log(&format!("trying to connect to {port} {name} USB VID:PID=0403:6001")),
            Diagnostic::log(&format!("Trying to instance {port} baud_rate: {baud}")),
            // The protocol's own prints for the unanswered read: the response is unpacked from its lone STX.
            Diagnostic::print(UNANSWERED),
            Diagnostic::print("readModelCode - failed"),
            Diagnostic::log("model_code 0"),
            Diagnostic::log("error while reading model code not dyadic"),
        ]
    }

    fn silent_port(port: &str) -> Vec<Diagnostic> {
        [silent_attempt(port, 9600), silent_attempt(port, 115_200)].concat()
    }

    /// The first line of a port/baud pair the scan then skips (the port is already in use).
    fn skipped(port: &str) -> Diagnostic {
        let name = port.rsplit('/').next().unwrap();
        Diagnostic::log(&format!("trying to connect to {port} {name} USB VID:PID=0403:6001"))
    }

    const EMPTY_RECORD: &str = "{'interface': None, 'com_port': '', 'sn': '', 'axis': '', 'speed': -1, 'accel': -1, \
                                'new_speed': None, 'new_accel': None}";

    fn found_record(port: &str, sn: &str) -> String {
        format!(
            "{{'interface': <backend.controllers.pytbmscom.TMBSCOMProtocol object at 0xADDR>, 'com_port': '{port}', \
             'sn': '{sn}', 'axis': '0', 'speed': -1, 'accel': -1, 'new_speed': None, 'new_accel': None}}"
        )
    }

    /// What an answering SCN6 gives at 9600 baud; `actuators` is the printed form of the records.
    fn found_attempt(port: &str, actuators: &str) -> Vec<Diagnostic> {
        let name = port.rsplit('/').next().unwrap();
        vec![
            Diagnostic::log(&format!("trying to connect to {port} {name} USB VID:PID=0403:6001")),
            Diagnostic::log(&format!("Trying to instance {port} baud_rate: 9600")),
            // The fixture's model-code reply is `R4808976450`: hexadecimal 808976450.
            Diagnostic::log("model_code 34503877712"),
            Diagnostic::log("correct model"),
            Diagnostic::print("Normal operation"),
            Diagnostic::log("set_son"),
            Diagnostic::print("done"),
            Diagnostic::print("seton is done"),
            Diagnostic::print("set iterface"),
            Diagnostic::print("set port no"),
            Diagnostic::print("set default axis"),
            Diagnostic::print("set sn"),
            Diagnostic::print("set comport"),
            Diagnostic::print(actuators),
            Diagnostic::print(&format!("done with {port}")),
            // The second baud rate of the port the record now holds.
            skipped(port),
        ]
    }

    #[test]
    fn prs200_dyadic_connect_diagnostics_follow_the_source_line_by_line() {
        // The Dyadic variant's connect on T1000-3M PRS200 (one PRS200 board and the Dyadic TL axis).
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        let diagnostics = mask_addresses(capture_diagnostics(|| rig.connect()));
        let expected = [
            vec![
                Diagnostic::log("starting"),
                Diagnostic::log("motion_switch is on and no error"),
                Diagnostic::log("number of controllers 2"),
                Diagnostic::log("disconnect"),
            ],
            silent_port(P1),
            silent_port(P2),
            found_attempt(TL, &format!("{{0: {}}}", found_record(TL, "00000011"))),
            vec![
                Diagnostic::print("homing "),
                Diagnostic::print("homing"),
                Diagnostic::print("done"),
                Diagnostic::print("homing completed"),
                Diagnostic::print("dialytic set {0: '/dev/ttyUSB3'}"),
                Diagnostic::print("Trying to connect PRS200 to /dev/ttyUSB5 ttyUSB5 USB VID:PID=0403:6001"),
                Diagnostic::print("prs200 a is connected"),
            ],
        ]
        .concat();
        assert_eq!(diagnostics, expected);
        assert!(rig.link.is_connected());
    }

    #[test]
    fn prs200_tl_surge_connect_diagnostics_follow_the_source_line_by_line() {
        // The TL/surge variant's connect on T1000-6M PRS200 RK: two Dyadic records, then board A. Record 0
        // ends on the later port; record 1 takes the port record 0 gave up.
        let mut rig = Rig::new("T1000-6M PRS200 RK", ports(P1, ""));
        let diagnostics = mask_addresses(capture_diagnostics(|| rig.connect()));
        let both = |first: &str, second: &str| format!("{{0: {first}, 1: {second}}}");
        let expected = [
            vec![
                Diagnostic::log("starting"),
                Diagnostic::log("motion_switch is on and no error"),
                Diagnostic::log("number of controllers 3"),
                Diagnostic::log("disconnect"),
                Diagnostic::print(&format!("temp_dict {}", both(EMPTY_RECORD, EMPTY_RECORD))),
            ],
            // Record 0.
            silent_port(P1),
            silent_port(P2),
            found_attempt(TL, &both(&found_record(TL, "00000011"), EMPTY_RECORD)),
            found_attempt(SURGE, &both(&found_record(SURGE, "00000022"), EMPTY_RECORD)),
            // Record 1.
            silent_port(P1),
            silent_port(P2),
            found_attempt(TL, &both(&found_record(SURGE, "00000022"), &found_record(TL, "00000011"))),
            vec![skipped(SURGE), skipped(SURGE)],
            vec![
                Diagnostic::log("homing 0"),
                Diagnostic::log("homing completed0"),
                Diagnostic::log("homing 1"),
                Diagnostic::log("homing completed1"),
                Diagnostic::print("homing completed"),
                Diagnostic::print("dyadic set {0: '/dev/ttyUSB4', 1: '/dev/ttyUSB3'}"),
                Diagnostic::print("Trying to connect PRS200 to /dev/ttyUSB5 ttyUSB5 USB VID:PID=0403:6001"),
                Diagnostic::print("prs200 a is connected"),
            ],
        ]
        .concat();
        assert_eq!(diagnostics, expected);
        assert!(rig.link.is_connected());
    }

    #[test]
    fn prs200_scan_without_a_tl_axis_returns_before_the_boards() {
        // An empty Dyadic TL port prints 'something is wrong', stores 'Not connected' and
        // empty motion ports, and ends the connect. The board loop is not reached.
        for model in ["T1000-3M PRS200", "T1000-6M PRS200 RK"] {
            let mut rig = Rig::new(model, ports(P1, P2));
            rig.host.devices.remove(TL);
            rig.host.devices.remove(SURGE);
            let prints = capture_prints(|| rig.connect());
            assert_eq!(prints.last().map(String::as_str), Some("something is wrong"), "{model}");
            assert!(!prints.iter().any(|line| line.starts_with("Trying to connect PRS200 to")), "{model}: {prints:?}");
            assert!(rig.boards.written.lock().unwrap().is_empty(), "{model}: no board port is opened");
            assert!(!rig.link.is_connected());
            assert!(rig.link.ports.is_empty());
            assert!(rig.startups.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn prs200_power_poll_prints_busy_and_error_steps() {
        // The power-on poll: a clear 'Main power ON' flag prints 'turning on' and polls again at once; a read
        // that is unanswered or not hexadecimal prints the error text; the
        // fifty polls end without a pause and the setup goes on.
        let status = request(&Command::Read(Register::Status));
        for (answer, line) in [
            (Some(reply('0', "R400000002")), "turning on"),
            (None, "error while powering on 'int' object is not subscriptable"),
            (
                Some(reply('0', "R4XYZ000000")),
                "error while powering on invalid literal for int() with base 16: b'XYZ000000'",
            ),
        ] {
            let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
            let mut device = with_velocity(scn6_device(0x11, 7), 5000, 80);
            device.replies.retain(|(command, _)| *command != status);
            if let Some(answer) = answer {
                device.replies.push((status.clone(), answer));
            }
            rig.host = std::mem::take(&mut rig.host).device(TL, device);
            let before = rig.host.now();
            let mut result = None;
            let prints = capture_prints(|| result = Some(rig.call(|driver, context| driver.connect(context))));
            let polls: Vec<&str> = prints
                .iter()
                .map(String::as_str)
                .skip_while(|printed| *printed != "Normal operation")
                .skip(1)
                .take_while(|printed| *printed != "seton is done")
                .filter(|printed| *printed != "readSTATflags - failed" && *printed != UNANSWERED)
                .collect();
            assert_eq!(polls, vec![line; 50], "{line}");
            assert!(prints.iter().any(|printed| printed == "set comport"), "the setup continues: {line}");
            assert!(!prints.iter().any(|printed| printed == "done"), "{line}");
            // The homing poll then reads the same reply once: without the completion flag it sleeps 1 s per
            // poll; an error ends it at once.
            let homing: Vec<&str> = prints
                .iter()
                .map(String::as_str)
                .skip_while(|printed| *printed != "homing ")
                .take_while(|printed| !printed.starts_with("dialytic set"))
                .filter(|printed| *printed != "readSTATflags - failed" && *printed != UNANSWERED)
                .collect();
            let slept = rig.host.now() - before;
            if line == "turning on" {
                assert_eq!(homing.len(), 2 + 50 + 1);
                assert!(homing[1..52].iter().all(|printed| *printed == "homing"));
                assert_eq!(homing[52], "homing completed");
                // Four silent attempts (P1 and P2 at both baud rates) wait out two 0.5 s reads each.
                assert_eq!(slept, Duration::from_secs(4 + 50), "only the homing poll pauses");
            } else {
                let error = line.replacen("error while powering on", "error while homing", 1);
                assert_eq!(homing, ["homing ", "homing", error.as_str(), "homing completed"]);
            }
            result.unwrap().unwrap();
            assert!(rig.link.is_connected(), "{line}");
        }
    }

    #[test]
    fn prs200_scan_errors_after_the_model_check_leave_connect_with_the_partial_record() {
        // Only the errors of the model code check are handled in the scan. The ALRM flags read, the alarm
        // commands, servo on and the serial number read are not: their value error ends
        // the connect. The record keeps what was assigned before the failing line.
        let alarm = request(&Command::Read(Register::Alarm));
        let serial = request(&Command::Read(Register::SerialNumber));
        for (command, published) in [(alarm, false), (serial, true)] {
            let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
            let mut device = scn6_device(0x11, 7);
            for (request, answer) in &mut device.replies {
                if *request == command {
                    *answer = reply('0', "R4ZZZZZZZZ");
                }
            }
            rig.host = std::mem::take(&mut rig.host).device(TL, device);
            let mut result = None;
            let prints = capture_prints(|| result = Some(rig.call(|driver, context| driver.connect(context))));
            let error = result.unwrap().expect_err("the error leaves connect");
            assert_eq!(error.to_string(), "ValueError: invalid literal for int() with base 16: b'ZZZZZZZZ'");
            assert!(matches!(error.root(), MotionError::Native(_)));
            assert!(!prints.iter().any(|line| line == "something is wrong" || line == "set comport"), "{prints:?}");
            assert_eq!(prints.iter().any(|line| line == "set sn"), published);
            assert!(rig.boards.written.lock().unwrap().is_empty());
            assert_eq!(rig.driver.tl_port(), "", "L0xee was not reached");
            let record = &rig.driver.assignment().unwrap().actuators[0];
            assert_eq!((record.key, record.com_port.as_str(), record.sn.as_str()), (0, "", ""));
            assert_eq!(record.axis, if published { "0" } else { "" });
        }
    }

    /// The lines the registry prints for an error of the driver's connect:
    /// `error trying to connect from motion helpers` with the message, and the traceback.
    fn helper_prints(model: &'static str, device: Option<crate::motion::testing::Device>) -> Vec<String> {
        use crate::motion::{ConnectPassInputs, DriverEnvironment, DriverKind, MotionRegistry};
        let mut rig = Rig::new(model, ports(P1, ""));
        match device {
            Some(device) => rig.host = std::mem::take(&mut rig.host).device(TL, device),
            None => {
                rig.host.devices.remove(SURGE);
            }
        }
        let (settings, activity, boards) = (rig.settings.clone(), rig.activity.clone(), rig.boards.clone());
        let mut registry =
            MotionRegistry::new(DriverEnvironment { windows_drivers: true, ..DriverEnvironment::srs_linux() });
        for kind in [DriverKind::Prs200Dyadic, DriverKind::Prs200DyadicTlSurge] {
            let (settings, activity, boards) = (settings.clone(), activity.clone(), boards.clone());
            registry.register(
                kind,
                Box::new(move || {
                    Box::new(Prs200Driver::with_board_ports(
                        settings.clone(),
                        activity.clone(),
                        Box::new(boards.clone()),
                    ))
                }),
            );
        }
        let inputs = ConnectPassInputs { model, motion_switch_on: true, auto_search: true, enable_debug: false };
        let prints = capture_prints(|| registry.connect_step(&inputs, &mut rig.host));
        let from = prints.iter().position(|line| line.starts_with("error trying to connect")).expect("the handler ran");
        prints[from..].to_vec()
    }

    fn with_reply(command: Vec<u8>, payload: &str) -> crate::motion::testing::Device {
        let mut device = scn6_device(0x11, 7);
        for (request, answer) in &mut device.replies {
            if *request == command {
                *answer = reply('0', payload);
            }
        }
        device
    }

    #[test]
    fn connect_exceptions_print_the_traceback_through_the_compiled_frames() {
        // The registry's connect calls the driver's connect; the unhandled Dyadic steps of the scan
        // fail through the line of their call in the driver's connect and the protocol operation (and
        // helper) that holds the failing conversion. The frames print no source text.
        const HEAD: &str = "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", \
                            line 100, in backend.controllers.motion_helpers.connect\n";
        let frame = |file: &str, line: u32, function: &str| {
            format!("  File \"backend/controllers/{file}.py\", line {line}, in backend.controllers.{file}.{function}\n")
        };
        let invalid = |text: &str| format!("invalid literal for int() with base 16: b'{text}'");
        let cases = [
            (
                request(&Command::Read(Register::Alarm)),
                vec![
                    frame("prs200_dyadic", 195, "connect"),
                    frame("pytbmscom", 595, "TMBSCOMProtocol.readALRMflags"),
                    frame("pytbmscom", 297, "TMBSCOMProtocol.parseALRM"),
                ],
                invalid("ZZZZZZZZ"),
            ),
            (
                request(&Command::Servo(true)),
                vec![
                    frame("prs200_dyadic", 203, "connect"),
                    frame("pytbmscom", 701, "TMBSCOMProtocol.set_son"),
                    frame("pytbmscom", 217, "TMBSCOMProtocol.parseADirectResponseFormat"),
                ],
                invalid("ZZ"),
            ),
            (
                request(&Command::Read(Register::SerialNumber)),
                vec![
                    frame("prs200_dyadic", 229, "connect"),
                    frame("pytbmscom", 551, "TMBSCOMProtocol.readSerialNumber"),
                ],
                invalid("ZZZZZZZZ"),
            ),
            (
                request(&Command::Homing("07".into())),
                vec![
                    frame("prs200_dyadic", 241, "connect"),
                    frame("pytbmscom", 741, "TMBSCOMProtocol.move_org"),
                    frame("pytbmscom", 217, "TMBSCOMProtocol.parseADirectResponseFormat"),
                ],
                invalid("ZZ"),
            ),
        ];
        for (command, frames, message) in cases {
            let prints = helper_prints("T1000-3M PRS200", Some(with_reply(command, "R4ZZZZZZZZ")));
            let traceback = format!("{HEAD}{}ValueError: {message}\n", frames.concat());
            assert_eq!(prints, [format!("error trying to connect from motion helpers {message}"), traceback]);
        }
        // A direct reply whose third field is not hexadecimal fails at the PI conversion.
        let prints = helper_prints("T1000-3M PRS200", Some(with_reply(request(&Command::Servo(true)), "Q10B00ZZ00")));
        assert!(
            prints[1].contains(&frame("pytbmscom", 221, "TMBSCOMProtocol.parseADirectResponseFormat")),
            "{prints:?}"
        );
        // TL/surge variant: the record without an interface has no deeper frame.
        let prints = helper_prints("T1000-6M PRS200 RK", None);
        let message = "'NoneType' object has no attribute 'move_org'";
        assert_eq!(
            prints,
            [
                format!("error trying to connect from motion helpers {message}"),
                format!("{HEAD}{}AttributeError: {message}\n", frame("prs200_dyadic_tl_surge", 246, "connect")),
            ]
        );
    }

    #[test]
    fn prs200_tl_surge_homing_raises_for_a_record_without_an_interface() {
        // The TL/surge variant's connect homes every record in turn (mode '07').
        // With one SCN6 for the two records, record 1
        // has no interface and the homing ends the connect with an error after record 0 was sent its homing.
        let mut rig = Rig::new("T1000-6M PRS200 RK", ports(P1, ""));
        rig.host.devices.remove(SURGE);
        let mut result = None;
        let prints = capture_prints(|| result = Some(rig.call(|driver, context| driver.connect(context))));
        let error = result.unwrap().expect_err("the error leaves connect");
        assert_eq!(error.to_string(), "AttributeError: 'NoneType' object has no attribute 'move_org'");
        assert!(matches!(error.root(), MotionError::Native(_)));
        assert!(rig.host.log(TL).written().contains(&pack(0, "o0700000000")));
        assert!(!prints.iter().any(|line| line == "homing completed" || line.starts_with("dyadic set")));
        assert!(rig.boards.written.lock().unwrap().is_empty());
        assert_eq!(rig.driver.tl_port(), TL);
        let actuators = rig.driver.assignment().unwrap().actuators;
        assert_eq!(actuators.len(), 2, "both records stay in parameters['actuators']");
        assert_eq!(
            (actuators[0].com_port.as_str(), actuators[0].sn.as_str(), actuators[0].axis.as_str()),
            (TL, "00000011", "0")
        );
        assert_eq!(
            (actuators[1].com_port.as_str(), actuators[1].sn.as_str(), actuators[1].axis.as_str()),
            ("", "", "")
        );
    }

    fn native_list(values: Vec<LegacyValue>) -> LegacyValue {
        LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(values)))
    }

    /// A test frame's idle overrides are assignments into the caller's own list (another holder sees 250 and
    /// the TL centre); the board frames reduce each value to 16 bits at any width.
    #[test]
    fn native_idle_overrides_are_written_into_the_shared_list() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        let mut list = native_list((1..=6).map(native::int).collect());
        let alias = list.clone();
        rig.call(|d, ctx| d.send_test_native(&mut list, ctx)).unwrap();
        let slots: Vec<_> = (0..6).map(|i| native::item(&alias, i).unwrap()).collect();
        assert_eq!(slots, [250, 250, 32768, 250, 250, 6].map(native::int));
        rig.activity.update(|a| a.motion_test_mode = true);
        let wide = |low: i64| LegacyValue::from_integer((num_bigint::BigInt::from(1) << 90) + low);
        let mut list = native_list(vec![wide(1), wide(2), native::int(3), wide(4), wide(5), native::int(6)]);
        rig.call(|d, ctx| d.send_test_native(&mut list, ctx)).unwrap();
        let a = positions(&rig.boards.written(P1));
        assert_eq!(a.last().unwrap(), &position_frame(3, [2, 1, 4, 5]));
    }

    /// The actuator positions of a stream frame: each corner entry takes its slot as a 64-bit float in the rescale.
    #[test]
    fn native_travel_map_overflow_raises_in_the_stroke_statement() {
        let mut rig = Rig::new("T1000-2M PRS200", ports(P1, ""));
        rig.connect();
        let huge = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 1100);
        let mut list =
            native_list(vec![native::int(0), huge, native::int(0), native::int(0), native::int(0), native::int(0)]);
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "OverflowError: int too large to convert to float");
        assert_eq!(error.frames().last().map(|f| (f.function, f.line)), Some(("map", 204)));
        assert!(rig.driver.corner_travel().is_none());
    }

    /// The Dyadic TL position, slot 2 rescaled from 0..65536 to 0..10000, carries the frame of the position pass.
    #[test]
    fn native_dyadic_position_map_carries_its_line() {
        let mut rig = Rig::new("T1000-3M PRS200", ports(P1, ""));
        rig.connect();
        let huge = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 1100);
        let mut list =
            native_list(vec![native::int(0), native::int(0), huge, native::int(0), native::int(0), native::int(0)]);
        let error = rig.call(|d, ctx| d.send_stream_native(&mut list, ctx)).unwrap_err();
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(&frames[frames.len() - 2..], [("send_actuator_pos", 376), ("map", 204)]);
    }
}

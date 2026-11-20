//! Actuon8 controllers, which share the `[THANOS]` settings section.
//!
//! An STM32 board behind an FTDI FT232R at 250000 8N1, reset into
//! its normal firmware through RTS/DTR, identified by `INF` (`Actuon8(M):…`), configured with `SETnn…`
//! commands and driven with a 21-byte frame `FF FF <status> <8 × big-endian u16> 0A 0D`. While a game runs but
//! no live telemetry feeds the platform, the heartbeat thread the first successful connect starts repeats
//! the last frame with status PAUSE ([`SharedActuon`]).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tracing::debug;

use super::clock::{Clock, SystemClock};
use super::driver_support::{native_is_ft232r, CornerTravel, HostActivity};
use super::native;
use super::{
    python, ActuatorPositions, DriverFactory, LinkState, LinkStatus, LiveSettings, MotionContext, MotionDriver,
    MotionError, SharedLink,
};
use crate::discovery::allowed_search_com_ports;
use crate::serial::{Link, LinkError};
use openrigcue_model::native_value::LegacyValue;

/// The baud rate of a board.
pub const BAUD: u32 = 250_000;
/// The write timeout of a board's port.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
/// The controller id: the first `:` field of the `INF` reply.
pub const CONTROLLER_ID: &str = "Actuon8(M)";
/// Attempts of the `INF` query and the pause between them.
const INF_ATTEMPTS: usize = 10;
const INF_RETRY: Duration = Duration::from_millis(450);
/// Attempts and pause of the other queries (`GET56`).
const DEFAULT_ATTEMPTS: usize = 1;
const DEFAULT_RETRY: Duration = Duration::from_millis(100);
/// Pause after each `SET` command.
const SET_PAUSE: Duration = Duration::from_millis(100);
/// Mid-stroke parking value (the last frame at start, and the frame sent when a test stops).
const PARK: i64 = 32768;
/// The heartbeat repeats the last frame once no live frame came for this long.
const LIVE_TIMEOUT: Duration = Duration::from_millis(100);
/// Spike-filter range sent before the levels.
const SPIKE_RANGE: i64 = 127;
/// Listener keys that count as "no live telemetry".
const IDLE_LISTENERS: [&str; 3] = ["none", "emulator", "test"];
/// The game key when no game runs.
const IDLE_GAME: &str = "none";
/// Game keys under which a test frame drives the platform.
const TEST_GAMES: [&str; 2] = [IDLE_GAME, "test"];
/// Default heartbeat rate when `refresh_rate` is missing or not positive.
const DEFAULT_RATE: i64 = 30;
/// Ports 5–8 can be reversed.
const FIRST_REVERSIBLE_PORT: usize = 5;
const SLOTS: usize = 8;

/// The run state byte of the position frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Run = 1,
    Pause = 2,
    Resume = 3,
}

/// The field names of a configuration reply, in reply order.
pub const CONFIG_FIELDS: [&str; 17] = [
    "cnt", "dpt", "pkp", "pks", "pkt", "sbp", "sbs", "sbt", "pck", "est", "ksm", "kgr", "kgl", "kge", "tkl", "tke",
    "mft",
];

/// Encoders and parsers of the Actuon8 protocol.
pub mod protocol {
    use super::*;

    /// `FF FF <state> <p1..p8 big-endian u16> 0A 0D`. Values wrap to 16 bits.
    pub fn position_frame(state: RunState, positions: [i64; 8]) -> [u8; 21] {
        position_frame_words(state, positions.map(|p| p as u16))
    }

    /// The frame of positions already reduced to 16 bits.
    pub fn position_frame_words(state: RunState, words: [u16; 8]) -> [u8; 21] {
        let mut frame = [0u8; 21];
        frame[0] = 0xFF;
        frame[1] = 0xFF;
        frame[2] = state as u8;
        for (i, p) in words.iter().enumerate() {
            frame[3 + 2 * i..5 + 2 * i].copy_from_slice(&p.to_be_bytes());
        }
        frame[19] = 0x0A;
        frame[20] = 0x0D;
        frame
    }

    /// `SET<param><value>` without terminator: saves one parameter.
    pub fn set_command(param: &str, value: &str) -> Vec<u8> {
        format!("SET{param}{value}").into_bytes()
    }

    /// The five spike-filter commands of a motion frame, in order: range 127, vertical level,
    /// traction-loss level (three digits each), then both switches on (`1`).
    pub fn spike_filter_commands(vertical: i64, horizontal: i64) -> [Vec<u8>; 5] {
        spike_filter_commands_kg(to_kg(vertical), to_kg(horizontal))
    }

    /// The five `SET` commands for levels already converted.
    pub fn spike_filter_commands_kg(kg: i64, tl: i64) -> [Vec<u8>; 5] {
        [
            set_command("15", &format!("{SPIKE_RANGE:03}")),
            set_command("16", &format!("{kg:03}")),
            set_command("18", &format!("{tl:03}")),
            set_command("17", "1"),
            set_command("19", "1"),
        ]
    }

    /// The level of a spike-filter value: the value clamped to 0..=65535, times 253 / 65535, rounded half to
    /// even, plus 1, kept within 1..=254.
    pub fn to_kg(value: i64) -> i64 {
        let v = value.clamp(0, 65535) as f64;
        let scaled = (v * 253.0 / 65535.0).round_ties_even() as i64;
        (scaled + 1).clamp(1, 254)
    }

    /// The first non-empty stripped line containing `:`, the line a query's reply is taken from.
    pub fn response_line(raw: &str) -> Option<&str> {
        raw.split("\r\n")
            .map(openrigcue_model::python_numeric::python_strip)
            .filter(|l| !l.is_empty())
            .find(|l| l.contains(':'))
    }

    /// The controller field of an `INF` reply (the part before the first `:`, which may be empty).
    pub fn controller_field(line: &str) -> Option<&str> {
        line.split(':').next()
    }

    /// A `GET56` reply field that cannot be read: missing, or not an integer.
    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    #[error("GET56 field {field} is missing or not an integer")]
    pub struct ConfigParseError {
        pub field: &'static str,
    }

    /// Parses a configuration reply: one whose first field is `CAP` or `CFG` yields all 17 fields (field `i`
    /// at position `i + 3`), or an error when any of them is missing or not an integer; any other reply yields an
    /// empty map. The number of fields is tested before the CAP/CFG prefix.
    pub fn parse_config(line: &str) -> Result<BTreeMap<&'static str, i64>, ConfigParseError> {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() <= 2 || !matches!(parts.first(), Some(&"CAP") | Some(&"CFG")) {
            return Ok(BTreeMap::new());
        }
        CONFIG_FIELDS
            .iter()
            .enumerate()
            .map(|(i, &field)| {
                parts.get(i + 3).and_then(|p| py_int(p)).map(|v| (field, v)).ok_or(ConfigParseError { field })
            })
            .collect()
    }

    /// Decimal integer text (surrounding whitespace, a sign, underscores between digits) that fits an `i64`.
    fn py_int(text: &str) -> Option<i64> {
        crate::motion::thanos::protocol::parse_python_int(text)
    }
}

/// A serial link whose RTS and DTR lines can be driven (needed for the STM32 reset).
pub trait ModemLink: Link {
    fn set_rts(&mut self, level: bool) -> Result<(), LinkError>;
    fn set_dtr(&mut self, level: bool) -> Result<(), LinkError>;
}

/// Opens Actuon8 ports (250000 8N1 with modem-line control, which the shared host does not offer).
pub trait ActuonPorts: Send {
    fn open(&mut self, path: &str) -> Result<Box<dyn ModemLink>, LinkError>;
}

/// Real serial ports, DTR asserted on open.
#[derive(Debug, Default)]
pub struct SystemActuonPorts;

impl ActuonPorts for SystemActuonPorts {
    fn open(&mut self, path: &str) -> Result<Box<dyn ModemLink>, LinkError> {
        let result = serialport::new(path, BAUD).timeout(WRITE_TIMEOUT).dtr_on_open(true).open();
        #[cfg(target_os = "linux")]
        crate::serial::record_open_result(path, result.as_ref().err());
        let port = result.map_err(|source| LinkError::Open { path: path.to_string(), source })?;
        Ok(Box::new(ActuonLink { port, name: path.to_string() }))
    }
}

struct ActuonLink {
    port: Box<dyn serialport::SerialPort>,
    name: String,
}

impl Link for ActuonLink {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        self.port.write_all(bytes)?;
        Ok(())
    }
    fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
        let waiting = usize::try_from(self.port.bytes_to_read()?).unwrap_or(usize::MAX);
        let mut buf = vec![0; waiting];
        if waiting > 0 {
            let read = self.port.read(&mut buf)?;
            buf.truncate(read);
        }
        Ok(buf)
    }
    fn clear_input(&mut self) -> Result<(), LinkError> {
        self.port.clear(serialport::ClearBuffer::Input)?;
        Ok(())
    }
    fn name(&self) -> &str {
        &self.name
    }
}

impl ModemLink for ActuonLink {
    fn set_rts(&mut self, level: bool) -> Result<(), LinkError> {
        self.port.write_request_to_send(level)?;
        Ok(())
    }
    fn set_dtr(&mut self, level: bool) -> Result<(), LinkError> {
        self.port.write_data_terminal_ready(level)?;
        Ok(())
    }
}

/// Settings re-read on every call (`[THANOS]` section, tune, cache).
#[derive(Debug, Clone, PartialEq)]
pub struct ActuonSettings {
    /// The raw values as the driver reads them; `None` keeps the typed values below.
    pub native: Option<NativeActuonConfig>,
    /// `com_port` (legacy `thanos_com_port`); empty means auto-scan.
    pub port: String,
    /// `filter` (legacy `send_spike_filter`, default yes).
    pub spike_filter_enabled: bool,
    /// `reverse_controller_port_5` … `_8`.
    pub reverse_ports: [bool; 4],
    /// `[MOTION_COMPENSATION] vertical_actuators_stroke` (mm).
    pub vertical_stroke_mm: i64,
    /// The tune's `thanos_spike_filter_horizontal` / `thanos_spike_filter_vertical` (default 0).
    pub spike_filter_horizontal: i64,
    pub spike_filter_vertical: i64,
    /// The port remembered from the last successful connection: when set, the only port the first scan tries.
    pub cached_port: Option<String>,
    /// `[MOTIONBOX] refresh_rate` (heartbeat rate).
    pub refresh_rate: i64,
    /// Whether the platform is Windows (the heartbeat rate is capped at 30 Hz only when false).
    pub windows: bool,
    /// Whether the motion switch is on, which the heartbeat thread reads on every pass.
    pub motion_switch_on: bool,
}

impl Default for ActuonSettings {
    fn default() -> Self {
        ActuonSettings {
            native: None,
            port: String::new(),
            spike_filter_enabled: true,
            reverse_ports: [false; 4],
            vertical_stroke_mm: 0,
            spike_filter_horizontal: 0,
            spike_filter_vertical: 0,
            cached_port: None,
            refresh_rate: DEFAULT_RATE,
            windows: true,
            motion_switch_on: false,
        }
    }
}

/// What a motion frame reads, raw: the tune's horizontal and vertical spike-filter values (0 when absent),
/// the manufacturer's spike-filter switch (`yes` when absent), the four
/// reverse switches of ports 5 to 8 and the
/// `MOTION_COMPENSATION.vertical_actuators_stroke` setting.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeActuonConfig {
    pub spike_filters: [LegacyValue; 2],
    pub filter_enabled: Result<bool, String>,
    pub reverse_ports: [Result<bool, String>; 4],
    pub vertical_stroke: crate::motion::native::NativeSetting,
}

/// The level of a raw spike-filter value: as an integer of any width clamped to 0..=65535, then the bounded rounding.
fn native_to_kg(value: &LegacyValue) -> Result<i64, MotionError> {
    let v = native::to_int(value)?;
    let v = native::max(&native::int(0), &native::min(&v, &native::int(65_535))?)?;
    Ok(protocol::to_kg(native::as_i64(&v).expect("clamped to 0..=65535")))
}

/// One open Actuon8 board.
pub struct Actuon8 {
    link: Box<dyn ModemLink>,
}

impl Actuon8 {
    pub fn new(link: Box<dyn ModemLink>) -> Self {
        Actuon8 { link }
    }

    pub fn port(&self) -> &str {
        self.link.name()
    }

    /// STM32 boot-pin sequence through the FTDI modem lines (BOOT0 = 0, pulse NRST), which resets
    /// the board into its normal firmware.
    pub fn reset_to_normal_firmware(&mut self, clock: &mut dyn Clock) -> Result<(), LinkError> {
        debug!("starting Actuon8 RTS/DTR normal firmware boot sequence");
        debug!("setRTS(True): BOOT0=0 normal firmware boot (FTDI inverts RTS)");
        self.link.set_rts(true)?;
        debug!("setDTR(False): NRST=1 reset released");
        self.link.set_dtr(false)?;
        clock.sleep(Duration::from_millis(50));
        debug!("setDTR(True): NRST=0 reset asserted");
        self.link.set_dtr(true)?;
        clock.sleep(Duration::from_millis(100));
        debug!("setDTR(False): NRST=1 reset released");
        self.link.set_dtr(false)?;
        clock.sleep(Duration::from_secs(1));
        debug!(port = self.port(), "Actuon8 reset complete; stable state BOOT0=0, NRST=1");
        Ok(())
    }

    /// A query: write the command (no terminator), wait, read what arrived and
    /// pick the first line with a `:`; repeat up to `attempts` times; `''` when nothing matched. A failed
    /// write is logged and ends the exchange with that error. A failed read
    /// also ends the exchange; only successful reads without a matching line are retried.
    fn command_line(
        &mut self,
        command: &str,
        attempts: usize,
        retry: Duration,
        clock: &mut dyn Clock,
    ) -> Result<String, LinkError> {
        for attempt in 1..=attempts {
            debug!(port = self.port(), "{command} attempt {attempt} writing {command}");
            if let Err(e) = self.link.write_all(command.as_bytes()) {
                debug!(
                    port = self.port(),
                    "{command} write failed on attempt {attempt}: {}",
                    python::link_error_str(&e)
                );
                return Err(e);
            }
            clock.sleep(retry);
            let raw = String::from_utf8_lossy(&self.link.read_available()?).into_owned();
            debug!(
                port = self.port(),
                "{command} attempt {attempt} raw response {}",
                openrigcue_model::python_repr::repr_str(&raw)
            );
            if let Some(line) = protocol::response_line(&raw) {
                debug!(port = self.port(), "{command} attempt {attempt} selected response line {line}");
                return Ok(line.to_string());
            }
        }
        debug!(port = self.port(), "{command} failed after {attempts} attempts");
        Ok(String::new())
    }

    /// The firmware text: `INF`, 10 attempts 0.45 s apart.
    pub fn firmware_line(&mut self, clock: &mut dyn Clock) -> Result<String, LinkError> {
        self.command_line("INF", INF_ATTEMPTS, INF_RETRY, clock)
    }

    /// The parsed configuration of the `GET56` query.
    pub fn config(&mut self, clock: &mut dyn Clock) -> Result<BTreeMap<&'static str, i64>, ConfigReadError> {
        let line = self.command_line("GET56", DEFAULT_ATTEMPTS, DEFAULT_RETRY, clock)?;
        debug!("raw GET56 response is {line}");
        let config = protocol::parse_config(&line)?;
        let entries = CONFIG_FIELDS
            .iter()
            .filter_map(|key| config.get(key).map(|value| format!("'{key}': {value}")))
            .collect::<Vec<_>>()
            .join(", ");
        debug!("parsed config is {{{entries}}}");
        Ok(config)
    }

    /// Saves a parameter: write the `SET` command, then pause 0.1 s. A failed write is
    /// logged and returns at once, without the pause.
    fn send_set(&mut self, parameter: &str, command: &[u8], clock: &mut dyn Clock) {
        let text = String::from_utf8_lossy(command);
        debug!(port = self.port(), "TX ASCII {text}");
        match self.link.write_all(command) {
            Ok(()) => clock.sleep(SET_PAUSE),
            Err(e) => {
                debug!(port = self.port(), "failed writing SET command {parameter}: {}", python::link_error_str(&e))
            }
        }
    }

    /// Sends a position frame: a failed write is only logged.
    pub fn send_position(&mut self, state: RunState, positions: [i64; 8]) {
        self.send_words(state, positions.map(|p| p as u16));
    }

    /// Sends a position frame of dynamic values: each reduced to the low 16 bits
    /// of any integer before the frame is written.
    pub fn send_position_native(&mut self, state: RunState, positions: &[LegacyValue; 8]) -> Result<(), MotionError> {
        let mut words = [0u16; 8];
        for (word, value) in words.iter_mut().zip(positions) {
            *word = native::c_uint16(value)?;
        }
        self.send_words(state, words);
        Ok(())
    }

    fn send_words(&mut self, state: RunState, words: [u16; 8]) {
        let frame = protocol::position_frame_words(state, words);
        let status = match state {
            RunState::Run => "CM_RUN",
            RunState::Pause => "CM_PAUSE",
            RunState::Resume => "CM_RESUME",
        };
        let hex = frame.iter().map(|byte| format!("{byte:02X}")).collect::<Vec<_>>().join(" ");
        debug!("TX v2 status={status} {hex}");
        if let Err(e) = self.link.write_all(&frame) {
            debug!(port = self.port(), "failed writing 21-byte position frame: {}", python::link_error_str(&e));
        }
    }
}

/// Why `GET56` gave no configuration: the serial exchange failed, or the reply could not be
/// parsed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigReadError {
    #[error(transparent)]
    Serial(#[from] LinkError),
    #[error(transparent)]
    Parse(#[from] protocol::ConfigParseError),
}

/// Outcome of the `INF` / `GET56` handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    /// The controller is an Actuon8; `config` is the parsed `GET56` reply.
    Accepted {
        config: BTreeMap<&'static str, i64>,
    },
    Rejected,
}

/// The handshake: a failed `INF` write, a controller field other than `Actuon8(M)`,
/// and a failed `GET56` write or parse all reject the board.
pub fn handshake(board: &mut Actuon8, clock: &mut dyn Clock) -> Handshake {
    debug!("checking Actuon8 connection");
    let line = match board.firmware_line(clock) {
        Ok(line) => line,
        Err(e) => {
            debug!(port = board.port(), "failed to read Actuon8 firmware response {e}");
            return Handshake::Rejected;
        }
    };
    debug!("firmware response is {line}");
    let controller = protocol::controller_field(&line).unwrap_or("");
    debug!("parsed controller field is {controller}");
    match controller {
        CONTROLLER_ID => match board.config(clock) {
            Ok(config) => {
                debug!("Actuon8 connection accepted");
                Handshake::Accepted { config }
            }
            Err(e) => {
                debug!(port = board.port(), "Actuon8 connected but failed to parse GET56 config {e}");
                Handshake::Rejected
            }
        },
        _ => {
            debug!("controller field is not Actuon8(M): {controller}");
            Handshake::Rejected
        }
    }
}

/// The Actuon driver.
pub struct ActuonDriver {
    settings: LiveSettings<ActuonSettings>,
    activity: LiveSettings<HostActivity>,
    ports: Box<dyn ActuonPorts>,
    /// The open board.
    board: Option<Actuon8>,
    /// Whether the next scan is the first one, which only tries the cached port.
    first_scan: bool,
    /// Whether the STM32 reset has run; it runs once per connect cycle.
    boot_reset_done: bool,
    /// The controller configuration as last parsed (empty until a `CAP`/`CFG` reply).
    config: BTreeMap<&'static str, i64>,
    /// The spike-filter values last seen.
    previous_filter: (i64, i64),
    /// The raw spike-filter values last seen.
    previous_native_filter: [LegacyValue; 2],
    /// The last frame sent.
    last_frame: [LegacyValue; 8],
    /// When the last live frame came.
    last_live: Duration,
    /// Whether the platform is paused.
    in_pause: bool,
    /// The motion test mode of the previous test frame.
    previous_test_mode: bool,
    /// Whether the heartbeat thread was started.
    heartbeat_started: bool,
    /// Port of the last successful connect (remembered in the cache for the next first scan).
    connected_port: Option<String>,
    travel: Option<CornerTravel>,
}

/// The USB identity the scan requires in a port's `hwid`.
const ACTUON_VID_PID: &str = "0403:6001";

impl ActuonDriver {
    pub fn new(settings: LiveSettings<ActuonSettings>, activity: LiveSettings<HostActivity>) -> Self {
        Self::with_ports(settings, activity, Box::new(SystemActuonPorts))
    }

    pub fn with_ports(
        settings: LiveSettings<ActuonSettings>,
        activity: LiveSettings<HostActivity>,
        ports: Box<dyn ActuonPorts>,
    ) -> Self {
        ActuonDriver {
            settings,
            activity,
            ports,
            board: None,
            first_scan: true,
            boot_reset_done: false,
            config: BTreeMap::new(),
            previous_filter: (0, 0),
            previous_native_filter: [native::int(0), native::int(0)],
            last_frame: std::array::from_fn(|_| native::int(PARK)),
            last_live: Duration::ZERO,
            in_pause: true,
            previous_test_mode: false,
            heartbeat_started: false,
            connected_port: None,
            travel: None,
        }
    }

    /// The registry factory and the shared state behind it (the driver its heartbeat thread shares).
    pub fn factory(
        settings: LiveSettings<ActuonSettings>,
        activity: LiveSettings<HostActivity>,
    ) -> (DriverFactory, SharedActuon) {
        let shared = SharedActuon::new(ActuonDriver::new(settings, activity));
        (shared.factory(), shared)
    }

    /// The port to store in the cache after a scan found the board.
    pub fn connected_port(&self) -> Option<&str> {
        self.connected_port.as_deref()
    }

    /// Parsed `GET56` configuration.
    pub fn config(&self) -> &BTreeMap<&'static str, i64> {
        &self.config
    }

    pub fn corner_travel(&self) -> Option<CornerTravel> {
        self.travel
    }

    /// Pause between heartbeat ticks: the rate is the refresh rate as an integer; a rate not above 0
    /// is 30; off Windows a rate above 30 is 30;
    /// the pause is `1.0 / rate`.
    pub fn heartbeat_interval(&self) -> Duration {
        let settings = self.settings.get();
        let mut rate = if settings.refresh_rate <= 0 { DEFAULT_RATE } else { settings.refresh_rate };
        if !settings.windows && rate > DEFAULT_RATE {
            rate = DEFAULT_RATE;
        }
        openrigcue_model::python_time::sleep_duration(1.0 / rate as f64)
            .expect("positive i64 heartbeat rate gives a valid timeout")
    }

    /// One iteration of the heartbeat loop (which runs on a thread at [`Self::heartbeat_interval`]).
    ///
    /// Nothing happens unless the heartbeat runs, the platform is connected with the motion switch on, a board
    /// is open and motion-test mode is off. While no game runs the platform is only
    /// marked paused. Otherwise the last frame is repeated with status PAUSE when the listener is
    /// `none`/`emulator`/`test` or no live frame came for more than 0.1 s.
    pub fn heartbeat_tick(&mut self, link: &LinkState, motion_switch_on: bool, clock: &dyn Clock) {
        if !self.heartbeat_started || !link.is_connected() || !motion_switch_on || self.board.is_none() {
            return;
        }
        let activity = self.activity.get().live();
        if activity.motion_test_mode {
            return;
        }
        if activity.game_running == IDLE_GAME {
            self.in_pause = true;
            return;
        }
        let idle_listener = IDLE_LISTENERS.contains(&activity.listener_running.as_str());
        let stale = clock.now().saturating_sub(self.last_live) > LIVE_TIMEOUT;
        if idle_listener || stale {
            self.in_pause = true;
            if let Some(board) = self.board.as_mut() {
                if let Err(e) = board.send_position_native(RunState::Pause, &self.last_frame) {
                    debug!("error sending Actuon8 heartbeat position {}", e.python_str());
                }
            }
        }
    }

    /// The run state of the next live frame.
    fn run_state(&mut self, activity: &HostActivity) -> RunState {
        if activity.motion_test_mode {
            self.in_pause = false;
            return RunState::Run;
        }
        if IDLE_LISTENERS.contains(&activity.listener_running.as_str()) {
            self.in_pause = true;
            return RunState::Pause;
        }
        if self.in_pause {
            self.in_pause = false;
            return RunState::Resume;
        }
        RunState::Run
    }

    /// Tries to connect a board on one port.
    fn try_port(&mut self, path: &str, ctx: &mut MotionContext<'_>) -> bool {
        debug!("Trying Actuon8 connection on {path}");
        let link = match self.ports.open(path) {
            Ok(link) => link,
            Err(e) => {
                debug!(port = path, "Failed to instantiate Actuon8 on this port {e}");
                self.board = None;
                return false;
            }
        };
        let mut board = Actuon8::new(link);
        let clock = ctx.host.clock();
        if !self.boot_reset_done {
            if let Err(e) = board.reset_to_normal_firmware(clock) {
                debug!(port = path, "Failed to instantiate Actuon8 on this port {e}");
                return false;
            }
        } else {
            debug!("skipping PCB reset (already attempted on a previous connect cycle)");
        }
        self.boot_reset_done = true;
        match handshake(&mut board, clock) {
            Handshake::Accepted { config } => {
                self.config = config;
                debug!("Actuon8 connected on {path}");
                self.board = Some(board);
                ctx.link.status = LinkStatus::Connected;
                ctx.link.ports.insert(0, path.to_string());
                self.heartbeat_started = true;
                true
            }
            Handshake::Rejected => {
                debug!("Actuon8 handshake failed on {path}; disconnecting");
                false
            }
        }
    }

    /// The auto-scan of a connect: each listed port is logged, then passed over when its `hwid` lacks
    /// `0403:6001`, when the allowed-ports setting rejects it or when another device holds it. On the first
    /// scan a cached port that is another port also skips it; a failed cache read counts as no cached
    /// port, so without a cache entry every port is tried. Returns whether a board connected.
    fn scan(&mut self, s: &ActuonSettings, ctx: &mut MotionContext<'_>) -> Result<bool, MotionError> {
        let allowed = ctx.host.allowed_com_ports();
        let claimed = ctx.host.claimed_ports();
        for port in ctx.host.serial_ports_checked()? {
            let path = port.path.as_str();
            let details = ctx.host.port_details(&port);
            debug!("Actuon8 scan sees port={path} description={} address={}", details.description, details.hwid_text());
            // A port whose hardware id lacks the USB identity is passed over.
            let connect_line = super::PyFrame::new("backend/controllers/actuon.py", "connect", 402);
            if !native_is_ft232r(&details).map_err(|e| e.in_frame(connect_line))? {
                debug!("skipping {path} because address is not {ACTUON_VID_PID}");
                continue;
            }
            if !allowed_search_com_ports(path, &allowed) {
                debug!("skipping {path} because allowed_search_com_ports returned False");
                continue;
            }
            if claimed.contains(path) {
                debug!("skipping {path} because the port is already in use");
                continue;
            }
            if self.first_scan {
                // A missing cache entry fails before the log line; only a stored port
                // is logged here.
                if let Some(cached) = &s.cached_port {
                    debug!("Actuon8 cached COM port is {cached}");
                    if cached != path {
                        debug!("skipping {path} because cached port is {cached}");
                        continue;
                    }
                }
            }
            if self.try_port(path, ctx) {
                self.connected_port = Some(port.path.clone());
                debug!(
                    "Actuon8 connection completed; motion_com_port={}",
                    super::scn6::axis::ports_repr(&ctx.link.ports)
                );
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A motion frame: spike-filter push when its values changed, then the position frame.
    /// Every failure is only logged; each spike-filter `SET` is sent even when an
    /// earlier one failed. The errors of the frame block are handled together ("error sending Actuon8 motor
    /// position"): ports 5 to 8 (65536 minus the slot for a reversed port), the last frame, the frame (each
    /// value reduced to 16 bits) and the corner rescales on the slots as they are, at any width.
    fn send_motor_position(&mut self, motors: &LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let s = self.settings.get();
        let activity = self.activity.get().live();
        let filter = (s.spike_filter_horizontal, s.spike_filter_vertical);
        let clock = ctx.host.clock();
        // The steps before the frame block: an error ends the motion frame before the last-seen values are stored.
        let push = match &s.native {
            Some(native) => {
                let mut changed = false;
                for (a, b) in native.spike_filters.iter().zip(&self.previous_native_filter) {
                    if !native::eq(a, b)? {
                        changed = true;
                        break;
                    }
                }
                if !self.config.is_empty() && changed && native.filter_enabled.clone().map_err(MotionError::Native)? {
                    let kg = native_to_kg(&native.spike_filters[1])?;
                    let tl = native_to_kg(&native.spike_filters[0])?;
                    Some((kg, tl))
                } else {
                    None
                }
            }
            None => (!self.config.is_empty() && self.previous_filter != filter && s.spike_filter_enabled)
                .then(|| (protocol::to_kg(filter.1), protocol::to_kg(filter.0))),
        };
        if let Some((kg, tl)) = push {
            if let Some(board) = self.board.as_mut() {
                for (parameter, command) in
                    ["15", "16", "18", "17", "19"].into_iter().zip(protocol::spike_filter_commands_kg(kg, tl))
                {
                    board.send_set(parameter, &command, clock);
                }
            }
        }
        let sent = (|| -> Result<(), MotionError> {
            let mut upper: [LegacyValue; 4] = std::array::from_fn(|_| LegacyValue::None);
            for (k, (value, &typed)) in upper.iter_mut().zip(&s.reverse_ports).enumerate() {
                let reversed = match &s.native {
                    Some(native) => native.reverse_ports[k].clone().map_err(MotionError::Native)?,
                    None => typed,
                };
                let slot = native::item(motors, FIRST_REVERSIBLE_PORT - 1 + k)?;
                *value = if reversed { native::sub(&native::int(65_536), &slot)? } else { slot };
            }
            self.last_live = clock.now();
            let mut frame: [LegacyValue; 8] = std::array::from_fn(|_| LegacyValue::None);
            for (index, value) in frame.iter_mut().take(4).enumerate() {
                *value = native::item(motors, index)?;
            }
            for (value, upper) in frame.iter_mut().skip(4).zip(upper) {
                *value = upper;
            }
            self.last_frame = frame.clone();
            let state = self.run_state(&activity);
            let board = self
                .board
                .as_mut()
                .ok_or_else(|| native::raised("AttributeError: 'NoneType' object has no attribute 'send_position'"))?;
            board.send_position_native(state, &frame)?;
            // The stroke setting as an integer, at any width.
            let stroke = match &s.native {
                Some(native) => native::to_int(&native.vertical_stroke.clone().map_err(MotionError::Native)?)?,
                None => native::int(s.vertical_stroke_mm),
            };
            let mut corners = [0.0; 4];
            for (corner, slot) in corners.iter_mut().zip([1, 2, 0, 3]) {
                let value = native::item(motors, slot)?;
                *corner = native::map([&value, &native::int(0), &native::int(65_535), &native::int(0), &stroke])?;
            }
            let [front_left, front_right, rear_left, rear_right] = corners;
            self.travel = Some(CornerTravel { front_left, front_right, rear_left, rear_right });
            Ok(())
        })();
        if let Err(e) = sent {
            debug!("error sending Actuon8 motor position {}", e.python_str());
        }
        self.previous_filter = filter;
        if let Some(native) = &s.native {
            self.previous_native_filter = native.spike_filters.clone();
        }
        Ok(())
    }

    /// Stops a test: park at mid-stroke with status PAUSE.
    fn stop_test(&mut self) {
        self.last_frame = std::array::from_fn(|_| native::int(PARK));
        self.in_pause = true;
        if let Some(board) = self.board.as_mut() {
            board.send_position(RunState::Pause, [PARK; SLOTS]);
        }
    }
}

impl MotionDriver for ActuonDriver {
    /// Connects: the configured port, or a scan of FT232R ports (the first scan only tries
    /// the cached port). When neither connects, the platform is "Not connected" with no ports and later scans
    /// try every port.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        debug!("starting Actuon8 connect");
        if self.boot_reset_done && ctx.link.ports.is_empty() {
            debug!("Actuon8 boot reset guard cleared because no motion COM port is registered");
            self.boot_reset_done = false;
        }
        let s = self.settings.get();
        let connected = if s.port.is_empty() {
            debug!("Actuon8 auto-scan starting");
            self.scan(&s, ctx)?
        } else {
            debug!("Actuon8 manual COM port configured: {}", s.port);
            self.try_port(&s.port, ctx)
        };
        if !connected {
            self.first_scan = false;
            debug!("Actuon8 not connected after scan");
            ctx.link.status = LinkStatus::NotConnected;
            ctx.link.ports.clear();
        }
        Ok(())
    }

    /// Disconnects: close the board, clear the reset guard.
    fn disconnect(&mut self, _ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.board = None;
        self.boot_reset_done = false;
        Ok(())
    }

    /// A stream frame.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_stream_native(&mut native::list_of(motors), ctx)
    }

    /// A stream frame on the caller's list (which is only read).
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        // An error of the motion frame is only logged at debug level ("send_stream failed").
        if let Err(e) = self.send_motor_position(motors, ctx) {
            debug!("send_stream failed {}", e.python_str());
        }
        Ok(())
    }

    /// A test frame: leaving test mode while the game key is `none` or `test`
    /// parks the platform; frames are sent only in test mode and only while the game key is `none` or `test`.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.send_test_native(&mut native::list_of(motors), ctx)
    }

    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let activity = self.activity.get().live();
        let idle = TEST_GAMES.contains(&activity.game_running.as_str());
        if self.previous_test_mode && !activity.motion_test_mode && idle {
            self.stop_test();
        }
        self.previous_test_mode = activity.motion_test_mode;
        if activity.motion_test_mode && idle {
            self.send_motor_position(motors, ctx)?;
        }
        Ok(())
    }

    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.travel.map(CornerTravel::to_positions)
    }

    fn actuators_travel(&self) -> Option<CornerTravel> {
        self.travel
    }

    fn describe(&self) -> String {
        match &self.board {
            Some(b) => format!("Actuon8 on {}", b.port()),
            None => "Actuon8 (not connected)".into(),
        }
    }
}

/// Starts the heartbeat thread with the
/// loop's body; tests keep it instead of running it.
pub type HeartbeatSpawner = Box<dyn FnMut(Box<dyn FnOnce() + Send>) + Send>;

/// The real spawner: a detached thread for the rest of the process.
fn spawn_heartbeat_thread(body: Box<dyn FnOnce() + Send>) {
    if let Err(e) = std::thread::Builder::new().name("_heartbeat_loop".into()).spawn(body) {
        tracing::error!("cannot start the Actuon8 heartbeat: {e}");
    }
}

/// The shared state of the Actuon driver: one [`ActuonDriver`] used by the motion registry and by the
/// heartbeat thread that a successful connect starts, once for the process.
#[derive(Clone)]
pub struct SharedActuon {
    driver: Arc<Mutex<ActuonDriver>>,
    /// The motion status, as the registry hands it to its driver.
    link: Arc<Mutex<SharedLink>>,
    /// Whether the heartbeat thread is started, and how it is started.
    heartbeat: Arc<Mutex<(bool, HeartbeatSpawner)>>,
}

impl SharedActuon {
    pub fn new(driver: ActuonDriver) -> Self {
        Self::with_heartbeat_spawner(driver, Box::new(spawn_heartbeat_thread))
    }

    /// The same handle starting its heartbeat thread through `spawner` (tests run the loop body by hand).
    pub fn with_heartbeat_spawner(driver: ActuonDriver, spawner: HeartbeatSpawner) -> Self {
        SharedActuon {
            driver: Arc::new(Mutex::new(driver)),
            link: Arc::new(Mutex::new(SharedLink::default())),
            heartbeat: Arc::new(Mutex::new((false, spawner))),
        }
    }

    /// A factory whose drivers all share this handle's state.
    pub fn factory(&self) -> DriverFactory {
        let shared = self.clone();
        Box::new(move || Box::new(shared.clone()))
    }

    fn lock(&self) -> MutexGuard<'_, ActuonDriver> {
        self.driver.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn link(&self) -> SharedLink {
        self.link.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The heartbeat loop: the pause computed once, then for ever: the pause, then
    /// one [`ActuonDriver::heartbeat_tick`] on the state as it is then.
    pub fn heartbeat_loop(&self) {
        let interval = self.lock().heartbeat_interval();
        let mut clock = SystemClock::default();
        loop {
            clock.sleep(interval);
            self.heartbeat_pass(&clock);
        }
    }

    /// One pass of the heartbeat loop after its pause.
    pub fn heartbeat_pass(&self, clock: &dyn Clock) {
        let link = self.link().get();
        let mut driver = self.lock();
        let motion_switch_on = driver.settings.get().motion_switch_on;
        driver.heartbeat_tick(&link, motion_switch_on, clock);
    }

    /// Starts the heartbeat: the thread is started by the first connect that accepts a board.
    fn start_heartbeat(&self) {
        if !self.lock().heartbeat_started {
            return;
        }
        let mut heartbeat = self.heartbeat.lock().unwrap_or_else(PoisonError::into_inner);
        if heartbeat.0 {
            return;
        }
        heartbeat.0 = true;
        let shared = self.clone();
        (heartbeat.1)(Box::new(move || shared.heartbeat_loop()));
    }

    /// The port to store in the cache after a scan found the board.
    pub fn connected_port(&self) -> Option<String> {
        self.lock().connected_port().map(str::to_string)
    }
}

impl MotionDriver for SharedActuon {
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let result = self.lock().connect(ctx);
        self.start_heartbeat();
        result
    }
    fn disconnect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().disconnect(ctx)
    }
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().send_stream(motors, ctx)
    }
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().send_test(motors, ctx)
    }
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().send_stream_native(motors, ctx)
    }
    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.lock().send_test_native(motors, ctx)
    }
    fn attach_link(&mut self, link: SharedLink) {
        *self.link.lock().unwrap_or_else(PoisonError::into_inner) = link;
    }
    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.lock().actuators_position()
    }
    fn actuators_travel(&self) -> Option<CornerTravel> {
        self.lock().actuators_travel()
    }
    fn describe(&self) -> String {
        self.lock().describe()
    }
}

#[cfg(test)]
mod tests {
    use super::protocol::*;
    use super::*;
    use crate::motion::driver_support::testing::{ftdi_port, ScriptedLink, Written};
    use crate::motion::motion_model;
    use crate::motion::testing::FakeHost;
    use std::sync::{Arc, Mutex};

    #[test]
    fn native_actuon_serial_set_errors_keep_parameter_keys_and_exception_text() {
        use crate::motion::driver_support::testing::capture_debug_messages;

        let mut settings = cached("/dev/ttyUSB2");
        settings.spike_filter_vertical = 1800;
        let mut rig = Rig::new("Actuon8(M)", settings);
        rig.connect();
        rig.board.failing.lock().unwrap().push(b"SET");
        rig.board.attempted.lock().unwrap().clear();
        let before = rig.host.now();
        let messages =
            capture_debug_messages(|| rig.call(|driver, context| driver.send_stream(&[1; 8], context)).unwrap());
        assert_eq!(
            &messages[..10],
            [
                "TX ASCII SET15127",
                "failed writing SET command 15: write timeout",
                "TX ASCII SET16008",
                "failed writing SET command 16: write timeout",
                "TX ASCII SET18001",
                "failed writing SET command 18: write timeout",
                "TX ASCII SET171",
                "failed writing SET command 17: write timeout",
                "TX ASCII SET191",
                "failed writing SET command 19: write timeout",
            ]
        );
        assert_eq!(messages.len(), 11);
        assert!(messages[10].starts_with("TX v2 status="));
        assert_eq!(rig.host.now(), before, "failed SET writes must not wait");
        assert_eq!(rig.board.attempted.lock().unwrap().len(), 6, "all five SET commands and the frame are attempted");
    }

    #[test]
    fn native_actuon_serial_timeout_messages_match_pyserial_at_both_catches() {
        use crate::motion::driver_support::testing::capture_debug_messages;
        use crate::motion::testing::FakeClock;

        struct TimeoutLink;
        impl Link for TimeoutLink {
            fn write_all(&mut self, _: &[u8]) -> Result<(), LinkError> {
                Err(LinkError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "backend timeout detail")))
            }
            fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
                panic!("a failed write must not read")
            }
            fn clear_input(&mut self) -> Result<(), LinkError> {
                Ok(())
            }
            fn name(&self) -> &str {
                "/dev/fake-actuon"
            }
        }
        impl ModemLink for TimeoutLink {
            fn set_rts(&mut self, _: bool) -> Result<(), LinkError> {
                Ok(())
            }
            fn set_dtr(&mut self, _: bool) -> Result<(), LinkError> {
                Ok(())
            }
        }
        let mut board = Actuon8::new(Box::new(TimeoutLink));
        let mut clock = FakeClock::default();
        let messages = capture_debug_messages(|| {
            assert!(board.firmware_line(&mut clock).is_err());
        });
        assert_eq!(messages, ["INF attempt 1 writing INF", "INF write failed on attempt 1: Write timeout"]);
        assert_eq!(clock.now(), Duration::ZERO);
        let messages = capture_debug_messages(|| board.send_position(RunState::Run, [0; 8]));
        assert_eq!(messages.len(), 2);
        assert!(messages[0].starts_with("TX v2 status=CM_RUN FF FF 01 "));
        assert_eq!(messages[1], "failed writing 21-byte position frame: Write timeout");
    }

    #[test]
    fn native_actuon_logs_position_bytes_before_even_a_failed_write() {
        use crate::motion::driver_support::testing::capture_debug_messages;

        let mut ports = Board::default();
        let mut board = Actuon8::new(ports.open("/dev/fake-actuon").unwrap());
        let positions = [1, -1, 65536, 0x1234, 0, 32768, 256, 255];
        for (state, name, status) in [
            (RunState::Run, "CM_RUN", "01"),
            (RunState::Pause, "CM_PAUSE", "02"),
            (RunState::Resume, "CM_RESUME", "03"),
        ] {
            let messages = capture_debug_messages(|| board.send_position(state, positions));
            assert_eq!(
                messages,
                [format!("TX v2 status={name} FF FF {status} 00 01 FF FF 00 00 12 34 00 00 80 00 01 00 00 FF 0A 0D")]
            );
        }
        assert_eq!(ports.written.lock().unwrap().len(), 3);
        ports.failing.lock().unwrap().push(&[0xff, 0xff]);
        let messages = capture_debug_messages(|| board.send_position(RunState::Run, positions));
        assert_eq!(
            messages.first().unwrap(),
            "TX v2 status=CM_RUN FF FF 01 00 01 FF FF 00 00 12 34 00 00 80 00 01 00 00 FF 0A 0D"
        );
        assert_eq!(messages.len(), 2, "the frame is logged before the write error");
        assert!(messages[1].starts_with("failed writing 21-byte position frame: "));
        assert_eq!(ports.written.lock().unwrap().len(), 3);
        assert_eq!(ports.attempted.lock().unwrap().len(), 4);
    }

    #[test]
    fn native_actuon_logs_parsed_config_in_original_field_order_only_on_success() {
        use crate::motion::driver_support::testing::capture_debug_messages;
        use crate::motion::testing::FakeClock;

        let mut ports = Board { config_line: Arc::new(Mutex::new(FULL_CFG.into())), ..Board::default() };
        let mut board = Actuon8::new(ports.open("/dev/fake-actuon").unwrap());
        let mut clock = FakeClock::default();
        let messages = capture_debug_messages(|| {
            assert_eq!(board.config(&mut clock).unwrap().len(), 17);
        });
        assert_eq!(messages.last().unwrap(), "parsed config is {'cnt': 1, 'dpt': 2, 'pkp': 3, 'pks': 4, 'pkt': 5, 'sbp': 6, 'sbs': 7, 'sbt': 8, 'pck': 9, 'est': 10, 'ksm': 11, 'kgr': 12, 'kgl': 13, 'kge': 14, 'tkl': 15, 'tke': 16, 'mft': 17}");
        assert!(messages[messages.len() - 2].starts_with("raw GET56 response is CFG:"));
        *ports.config_line.lock().unwrap() = "GET:x:y:10:20".into();
        let messages = capture_debug_messages(|| assert!(board.config(&mut clock).unwrap().is_empty()));
        assert_eq!(messages.last().unwrap(), "parsed config is {}");
        for invalid in ["CFG:x:y:10:20:30".to_owned(), FULL_CFG.replace(":5:", ":five:")] {
            *ports.config_line.lock().unwrap() = invalid;
            let messages = capture_debug_messages(|| assert!(board.config(&mut clock).is_err()));
            assert!(messages.last().unwrap().starts_with("raw GET56 response is CFG:"));
            assert!(!messages.iter().any(|message| message.starts_with("parsed config is ")));
        }
    }

    #[test]
    fn native_actuon_short_config_prefix_is_ignored_before_field_parsing() {
        use crate::motion::driver_support::testing::capture_debug_messages;
        use crate::motion::testing::FakeClock;

        for reply in ["CAP", "CFG", "CAP:", "CFG:", "CAP:x", "CFG:x"] {
            assert_eq!(parse_config(reply), Ok(BTreeMap::new()), "{reply:?}");
            let mut ports = Board { config_line: Arc::new(Mutex::new(reply.into())), ..Board::default() };
            let mut board = Actuon8::new(ports.open("/dev/fake-actuon").unwrap());
            let messages = capture_debug_messages(|| {
                assert!(board.config(&mut FakeClock::default()).unwrap().is_empty());
            });
            assert_eq!(messages.last().unwrap(), "parsed config is {}");
        }
        for reply in ["CAP:x:y", "CFG:x:y"] {
            assert_eq!(parse_config(reply), Err(ConfigParseError { field: "cnt" }));
        }
    }

    #[test]
    fn position_frame_layout() {
        // The position frame: FF FF status, eight big-endian words, 0A 0D (21 bytes).
        let f = position_frame(RunState::Resume, [1, 2, 3, 4, 5, 6, 7, 65536]);
        assert_eq!(f.len(), 21);
        assert_eq!(&f[..5], &[0xFF, 0xFF, 3, 0, 1]);
        assert_eq!(&f[17..], &[0, 0, 0x0A, 0x0D], "65536 wraps to 0 (c_uint16)");
    }

    #[test]
    fn to_kg_matches_python() {
        // The level: v * 253 / 65535 rounded, plus 1, kept within 1..=254.
        assert_eq!(to_kg(0), 1);
        assert_eq!(to_kg(65535), 254);
        assert_eq!(to_kg(1800), 8);
        assert_eq!(to_kg(-5), 1);
        assert_eq!(to_kg(100_000), 254);
        // Either side of the 1.5 rounding boundary (v * 253 / 65535 = 1.4979 and 1.5018).
        assert_eq!(to_kg(388), 2);
        assert_eq!(to_kg(389), 3);
    }

    #[test]
    fn spike_filter_commands_are_set_strings() {
        // The spike-filter commands: SET15127, SET16<kg>, SET18<tl>, SET171, SET191.
        let c = spike_filter_commands(1800, 0);
        assert_eq!(c[0], b"SET15127");
        assert_eq!(c[1], b"SET16008");
        assert_eq!(c[2], b"SET18001");
        assert_eq!(c[3], b"SET171");
        assert_eq!(c[4], b"SET191");
    }

    #[test]
    fn response_line_selection() {
        // A query's reply: first stripped non-empty line containing ':'.
        assert_eq!(response_line("\r\nboot\r\n INF:Actuon8(M):v1.2 \r\n"), Some("INF:Actuon8(M):v1.2"));
        for space in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}', '\u{85}', '\u{2000}', '\u{2028}', '\u{3000}'] {
            let raw = format!("{space}\r\n{space}CFG:56:x:1:2:3:4:5:6:7:8:9:10:11:12:13:14:15:16:17{space}\r\n");
            let selected = response_line(&raw).unwrap();
            assert_eq!(selected, FULL_CFG, "U+{:04X}", u32::from(space));
            assert_eq!(parse_config(selected).unwrap().len(), 17);
        }
        assert_eq!(response_line("\0INF:Actuon8(M):v\0"), Some("\0INF:Actuon8(M):v\0"));
        assert_eq!(response_line("noise\r\n"), None);
        assert_eq!(controller_field("Actuon8(M):v1.2"), Some("Actuon8(M)"));
        assert_eq!(controller_field("INF:Actuon8(M):v1.2"), Some("INF"));
        assert_eq!(controller_field(""), Some(""));
    }

    /// A `GET56` reply carrying the 17 fields 1..=17.
    const FULL_CFG: &str = "CFG:56:x:1:2:3:4:5:6:7:8:9:10:11:12:13:14:15:16:17";

    #[test]
    fn gen_config_dict_reads_cap_and_cfg_replies_only() {
        // The configuration: field i is the integer at position i + 3 for CAP/CFG replies, else an empty map.
        let c = parse_config(FULL_CFG).unwrap();
        assert_eq!(c.len(), 17);
        assert_eq!(c.get("cnt"), Some(&1));
        assert_eq!(c.get("mft"), Some(&17));
        let c = parse_config(&FULL_CFG.replacen("CFG", "CAP", 1)).unwrap();
        assert_eq!(c.get("pkp"), Some(&3));
        assert_eq!(parse_config("GET:x:y:10:20"), Ok(BTreeMap::new()));
        assert_eq!(parse_config(""), Ok(BTreeMap::new()));
    }

    #[test]
    fn gen_config_dict_raises_on_a_missing_or_bad_field() {
        // No per-field guard: an error for a short reply and for a non-integer field.
        assert_eq!(parse_config("CFG:x:y:10:20:30"), Err(ConfigParseError { field: "pks" }));
        assert_eq!(parse_config(&FULL_CFG.replace(":5:", ":five:")), Err(ConfigParseError { field: "pkt" }));
        // Integer text may have surrounding whitespace and digit-separating underscores.
        assert_eq!(parse_config(&FULL_CFG.replace(":10:", ": 1_0 :")).unwrap()["est"], 10);
    }

    /// An Actuon8 behind a scripted link that records modem-line changes.
    #[derive(Clone, Default)]
    struct Board {
        written: Written,
        lines: Arc<Mutex<Vec<(&'static str, bool)>>>,
        controller: &'static str,
        /// The `GET56` reply line.
        config_line: Arc<Mutex<String>>,
        /// Paths whose open fails.
        fail_open: Arc<Mutex<Vec<String>>>,
        /// Writes starting with one of these prefixes fail.
        failing: Arc<Mutex<Vec<&'static [u8]>>>,
        /// Reads after one of these commands fail.
        failing_reads: Arc<Mutex<Vec<&'static [u8]>>>,
        /// Every write attempted, failed ones included.
        attempted: Written,
    }

    struct BoardLink {
        inner: ScriptedLink,
        lines: Arc<Mutex<Vec<(&'static str, bool)>>>,
        failing: Arc<Mutex<Vec<&'static [u8]>>>,
        failing_reads: Arc<Mutex<Vec<&'static [u8]>>>,
        attempted: Written,
    }
    impl Link for BoardLink {
        fn write_all(&mut self, b: &[u8]) -> Result<(), LinkError> {
            self.attempted.lock().unwrap().push(b.to_vec());
            if self.failing.lock().unwrap().iter().any(|p| b.starts_with(p)) {
                return Err(LinkError::Io(std::io::Error::other("write timeout")));
            }
            self.inner.write_all(b)
        }
        fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
            let attempted = self.attempted.lock().unwrap();
            if attempted.last().is_some_and(|command| {
                self.failing_reads.lock().unwrap().iter().any(|prefix| command.starts_with(prefix))
            }) {
                return Err(LinkError::Io(std::io::Error::other("read failed")));
            }
            self.inner.read_available()
        }
        fn clear_input(&mut self) -> Result<(), LinkError> {
            self.inner.clear_input()
        }
        fn name(&self) -> &str {
            self.inner.name()
        }
    }
    impl ModemLink for BoardLink {
        fn set_rts(&mut self, level: bool) -> Result<(), LinkError> {
            self.lines.lock().unwrap().push(("rts", level));
            Ok(())
        }
        fn set_dtr(&mut self, level: bool) -> Result<(), LinkError> {
            self.lines.lock().unwrap().push(("dtr", level));
            Ok(())
        }
    }

    impl ActuonPorts for Board {
        fn open(&mut self, path: &str) -> Result<Box<dyn ModemLink>, LinkError> {
            if self.fail_open.lock().unwrap().iter().any(|p| p == path) {
                return Err(LinkError::Io(std::io::Error::other("busy")));
            }
            let controller = self.controller;
            let config_line = self.config_line.clone();
            let mut inner = ScriptedLink::new(path, move |cmd| match cmd {
                b"INF" => format!("\r\n{controller}:v2\r\n").into_bytes(),
                b"GET56" => format!("{}\r\n", config_line.lock().unwrap()).into_bytes(),
                _ => Vec::new(),
            });
            inner.written = self.written.clone();
            Ok(Box::new(BoardLink {
                inner,
                lines: self.lines.clone(),
                failing: self.failing.clone(),
                failing_reads: self.failing_reads.clone(),
                attempted: self.attempted.clone(),
            }))
        }
    }

    struct Rig {
        driver: ActuonDriver,
        host: FakeHost,
        link: LinkState,
        board: Board,
        activity: LiveSettings<HostActivity>,
    }

    impl Rig {
        fn new(controller: &'static str, settings: ActuonSettings) -> Self {
            let mut host = FakeHost::default();
            host.ports.push(ftdi_port("/dev/ttyUSB2"));
            let board = Board { controller, config_line: Arc::new(Mutex::new(FULL_CFG.into())), ..Board::default() };
            let settings = LiveSettings::new(settings);
            let activity = LiveSettings::new(HostActivity::default());
            Rig {
                driver: ActuonDriver::with_ports(settings.clone(), activity.clone(), Box::new(board.clone())),
                host,
                link: LinkState::default(),
                board,
                activity,
            }
        }
        fn call<R>(&mut self, f: impl FnOnce(&mut ActuonDriver, &mut MotionContext<'_>) -> R) -> R {
            let spec = motion_model("4 Actuators [2 Front + 2 Rear]", false).unwrap();
            let mut ctx = MotionContext { model: spec, link: &mut self.link, host: &mut self.host };
            f(&mut self.driver, &mut ctx)
        }
        fn connect(&mut self) {
            self.link.ports.clear();
            self.call(|d, ctx| d.connect(ctx)).unwrap();
        }
        fn written(&self) -> Vec<Vec<u8>> {
            self.board.written.lock().unwrap().clone()
        }
    }

    fn cached(port: &str) -> ActuonSettings {
        ActuonSettings { cached_port: Some(port.into()), vertical_stroke_mm: 100, ..ActuonSettings::default() }
    }

    #[test]
    fn connect_resets_the_stm32_and_accepts_actuon8() {
        // Connecting a port resets the board (RTS on, DTR off/on/off), then runs the INF and GET56 handshake.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        assert!(rig.link.is_connected());
        assert_eq!(rig.link.ports.get(&0).map(String::as_str), Some("/dev/ttyUSB2"));
        assert_eq!(*rig.board.lines.lock().unwrap(), [("rts", true), ("dtr", false), ("dtr", true), ("dtr", false)]);
        assert_eq!(rig.written()[..2], [b"INF".to_vec(), b"GET56".to_vec()]);
        assert_eq!(rig.driver.config().get("cnt"), Some(&1));
        assert_eq!(rig.driver.connected_port(), Some("/dev/ttyUSB2"));
    }

    #[test]
    fn native_pnp_actuon_membership_fails_before_allow_list_and_reset() {
        let mut rig = Rig::new("ACTUON8", cached("/dev/ttyUSB2"));
        let port = rig.host.ports[0].clone();
        let mut details = crate::discovery::ListPortInfo::from_listing(&port);
        details.hwid = None;
        rig.host.listed_details.insert(port.path.clone(), details);
        rig.host.allowed = "/dev/unrelated".into();
        let error = rig.call(|driver, ctx| driver.connect(ctx)).unwrap_err();
        assert_eq!(error.to_string(), "TypeError: argument of type 'NoneType' is not iterable");
        // A port whose hardware id lacks the USB identity is passed over.
        assert_eq!(
            error.python_traceback().unwrap(),
            "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
             backend.controllers.motion_helpers.connect\n  File \"backend/controllers/actuon.py\", line 402, in \
             backend.controllers.actuon.connect\nTypeError: argument of type 'NoneType' is not iterable\n"
        );
        assert!(rig.driver.first_scan);
        assert!(!rig.driver.boot_reset_done);
        assert!(rig.written().is_empty());
        assert!(rig.board.attempted.lock().unwrap().is_empty());
    }

    #[test]
    fn actuon_first_reply_field_accepts_the_original_controller_first_reply() {
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        assert!(rig.link.is_connected());
        assert_eq!(rig.written()[..2], [b"INF".to_vec(), b"GET56".to_vec()]);
    }

    #[test]
    fn actuon_first_reply_field_rejects_an_inf_prefix_before_get56() {
        let mut rig = Rig::new("INF:Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        assert!(!rig.link.is_connected());
        assert_eq!(rig.written(), [b"INF".to_vec()]);
    }

    #[test]
    fn first_scan_only_tries_the_cached_port() {
        // Connect: the first scan skips ports other than the cached one.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB9"));
        rig.connect();
        assert!(!rig.link.is_connected());
        assert!(rig.written().is_empty());
        rig.connect();
        assert!(rig.link.is_connected(), "second scan tries every FT232R port");
    }

    #[test]
    fn first_scan_without_a_cached_port_tries_every_port() {
        // Connect: a failed cache read counts as no cached port;
        // only a cached port that is another port skips the port. A
        // first scan without a cache entry therefore tries the port at once.
        let mut rig = Rig::new("Actuon8(M)", ActuonSettings { vertical_stroke_mm: 100, ..ActuonSettings::default() });
        rig.connect();
        assert!(rig.link.is_connected(), "the first pass already tries the port");
        assert_eq!(rig.driver.connected_port(), Some("/dev/ttyUSB2"));
    }

    #[test]
    fn scan_logs_each_port_and_its_skip_reason() {
        // Connect: one 'scan sees' line per listed port, a 'skipping' line with the reason for each
        // port passed over, the cached port on the first scan, and the completion line with the motion ports.
        use crate::motion::driver_support::testing::capture_debug_messages;

        let scan_lines = |messages: Vec<String>| -> Vec<String> {
            messages
                .into_iter()
                .filter(|message| {
                    message.starts_with("Actuon8 scan sees")
                        || message.starts_with("skipping /dev/")
                        || message.starts_with("Actuon8 cached COM port")
                        || message.starts_with("Actuon8 connection completed")
                        || message.starts_with("Actuon8 auto-scan")
                        || message.starts_with("Actuon8 not connected")
                })
                .collect()
        };
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB9"));
        rig.host
            .ports
            .insert(0, crate::discovery::PortInfo { path: "/dev/ttyS0".into(), stable_path: None, usb: None });
        rig.host.ports.push(ftdi_port("/dev/ttyUSB3"));
        rig.host.ports.push(ftdi_port("/dev/ttyUSB4"));
        rig.host.allowed = "/dev/ttyUSB2,/dev/ttyUSB4,/dev/ttyS0".into();
        rig.host.claimed.belt = Some("/dev/ttyUSB4".into());
        let first = scan_lines(capture_debug_messages(|| rig.connect()));
        assert_eq!(
            first,
            [
                "Actuon8 auto-scan starting",
                "Actuon8 scan sees port=/dev/ttyS0 description=n/a address=n/a",
                "skipping /dev/ttyS0 because address is not 0403:6001",
                "Actuon8 scan sees port=/dev/ttyUSB2 description=ttyUSB2 address=USB VID:PID=0403:6001",
                "Actuon8 cached COM port is /dev/ttyUSB9",
                "skipping /dev/ttyUSB2 because cached port is /dev/ttyUSB9",
                "Actuon8 scan sees port=/dev/ttyUSB3 description=ttyUSB3 address=USB VID:PID=0403:6001",
                "skipping /dev/ttyUSB3 because allowed_search_com_ports returned False",
                "Actuon8 scan sees port=/dev/ttyUSB4 description=ttyUSB4 address=USB VID:PID=0403:6001",
                "skipping /dev/ttyUSB4 because the port is already in use",
                "Actuon8 not connected after scan",
            ]
        );
        let second = scan_lines(capture_debug_messages(|| rig.connect()));
        assert_eq!(
            second,
            [
                "Actuon8 auto-scan starting",
                "Actuon8 scan sees port=/dev/ttyS0 description=n/a address=n/a",
                "skipping /dev/ttyS0 because address is not 0403:6001",
                "Actuon8 scan sees port=/dev/ttyUSB2 description=ttyUSB2 address=USB VID:PID=0403:6001",
                "Actuon8 connection completed; motion_com_port={0: '/dev/ttyUSB2'}",
            ]
        );
        assert!(rig.link.is_connected());
    }

    #[test]
    fn other_controllers_are_rejected() {
        let mut rig = Rig::new("Thanos", cached("/dev/ttyUSB2"));
        rig.connect();
        assert!(!rig.link.is_connected());
        // The reply has a ':' line, so INF is not retried; its controller field decides.
        assert_eq!(rig.written().iter().filter(|w| *w == b"INF").count(), 1);
    }

    #[test]
    fn stream_states_pause_resume_run() {
        // The run state: idle listener → PAUSE; first live frame → RESUME; then RUN.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        let m = [100, 200, 300, 400, 500, 600, 700, 800, 0, 0, 0, 0];
        rig.call(|d, ctx| d.send_stream(&m, ctx)).unwrap();
        rig.activity.update(|a| a.listener_running = "dirt".into());
        rig.call(|d, ctx| d.send_stream(&m, ctx)).unwrap();
        rig.call(|d, ctx| d.send_stream(&m, ctx)).unwrap();
        let frames: Vec<u8> = rig.written().iter().filter(|w| w.len() == 21).map(|w| w[2]).collect();
        assert_eq!(frames, [2, 3, 1]);
        let t = rig.driver.corner_travel().unwrap();
        // The actuator positions: FL from slot 1, FR from 2, RL from 0, RR from 3.
        assert!((t.front_left - 200.0 * 100.0 / 65535.0).abs() < 1e-9);
        assert!((t.rear_left - 100.0 * 100.0 / 65535.0).abs() < 1e-9);
    }

    #[test]
    fn reverse_switches_and_spike_filter_push() {
        // Ports 5..8 reversed as 65536 - m; spike filter sent once when the controller configuration is known and the values changed.
        let mut s = cached("/dev/ttyUSB2");
        s.reverse_ports = [true, false, false, false];
        s.spike_filter_vertical = 1800;
        let mut rig = Rig::new("Actuon8(M)", s);
        rig.connect();
        let m = [1, 2, 3, 4, 1000, 6, 7, 8];
        rig.call(|d, ctx| d.send_stream(&m, ctx)).unwrap();
        rig.call(|d, ctx| d.send_stream(&m, ctx)).unwrap();
        let w = rig.written();
        assert_eq!(w.iter().filter(|c| *c == b"SET16008").count(), 1);
        let frame = w.iter().rev().find(|f| f.len() == 21).unwrap();
        assert_eq!(&frame[11..13], &64536u16.to_be_bytes());
    }

    /// The reset into the normal firmware: 50 ms + 100 ms + 1 s of pauses.
    const RESET_TIME: Duration = Duration::from_millis(1150);

    #[test]
    fn failed_inf_write_reraises_without_waiting_or_reading() {
        // A query's write failure is logged and returned, so the 0.45 s pause and the read are skipped
        // and no further attempt follows; the handshake handles it ('failed to read Actuon8 firmware
        // response') and rejects the board.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.board.failing.lock().unwrap().push(b"INF");
        let t0 = rig.host.now();
        rig.connect();
        assert!(!rig.link.is_connected());
        assert_eq!(rig.host.now() - t0, RESET_TIME);
        assert_eq!(*rig.board.attempted.lock().unwrap(), [b"INF".to_vec()]);
    }

    #[test]
    fn failed_get56_write_rejects_the_board() {
        // The write failure of the configuration query rejects the board in the handshake's second step.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.board.failing.lock().unwrap().push(b"GET56");
        let t0 = rig.host.now();
        rig.connect();
        assert!(!rig.link.is_connected());
        assert!(rig.driver.config().is_empty());
        // Only the successful INF attempt waited (0.45 s); the GET56 write failed before its 0.1 s pause.
        assert_eq!(rig.host.now() - t0, RESET_TIME + INF_RETRY);
        assert_eq!(*rig.board.attempted.lock().unwrap(), [b"INF".to_vec(), b"GET56".to_vec()]);
    }

    #[test]
    fn failed_inf_read_rejects_the_board_without_retrying() {
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.board.failing_reads.lock().unwrap().push(b"INF");
        let t0 = rig.host.now();
        rig.connect();
        assert!(!rig.link.is_connected());
        assert_eq!(rig.host.now() - t0, RESET_TIME + INF_RETRY);
        assert_eq!(*rig.board.attempted.lock().unwrap(), [b"INF".to_vec()]);
    }

    #[test]
    fn failed_get56_read_rejects_the_board() {
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.board.failing_reads.lock().unwrap().push(b"GET56");
        let t0 = rig.host.now();
        rig.connect();
        assert!(!rig.link.is_connected());
        assert!(rig.driver.config().is_empty());
        assert_eq!(rig.host.now() - t0, RESET_TIME + INF_RETRY + DEFAULT_RETRY);
        assert_eq!(*rig.board.attempted.lock().unwrap(), [b"INF".to_vec(), b"GET56".to_vec()]);
    }

    #[test]
    fn spike_filter_sets_are_all_sent_and_failed_ones_skip_the_pause() {
        // A parameter save whose write fails logs 'failed writing SET command' and returns
        // false without the 0.1 s pause; a motion frame sends all five commands without testing results.
        let mut s = cached("/dev/ttyUSB2");
        s.spike_filter_vertical = 1800;
        let mut rig = Rig::new("Actuon8(M)", s);
        rig.connect();
        rig.board.attempted.lock().unwrap().clear();
        rig.board.failing.lock().unwrap().push(b"SET15");
        rig.board.failing.lock().unwrap().push(b"SET18");
        let t0 = rig.host.now();
        rig.call(|d, ctx| d.send_stream(&[1; 8], ctx)).unwrap();
        let attempted = rig.board.attempted.lock().unwrap().clone();
        let sets: Vec<&[u8]> = attempted.iter().filter(|w| w.starts_with(b"SET")).map(Vec::as_slice).collect();
        assert_eq!(sets, [&b"SET15127"[..], b"SET16008", b"SET18001", b"SET171", b"SET191"]);
        // Three successful SET writes pause 0.1 s each.
        assert_eq!(rig.host.now() - t0, 3 * SET_PAUSE);
        assert_eq!(attempted.last().map(Vec::len), Some(21));
    }

    /// The 21-byte position frames written so far.
    fn position_frames(rig: &Rig) -> Vec<Vec<u8>> {
        rig.written().into_iter().filter(|w| w.len() == 21).collect()
    }

    #[test]
    fn heartbeat_only_marks_pause_while_no_game_runs() {
        // The heartbeat: no game running → the platform is marked paused and nothing is sent.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        rig.activity.update(|a| a.listener_running = "dirt".into());
        rig.call(|d, ctx| d.send_stream(&[1000; 8], ctx)).unwrap();
        assert!(!rig.driver.in_pause, "a live frame resumes");
        let sent = position_frames(&rig).len();
        rig.host.clock.sleep(Duration::from_secs(1));
        let link = rig.link.clone();
        rig.driver.heartbeat_tick(&link, true, &rig.host.clock);
        assert_eq!(position_frames(&rig).len(), sent, "nothing is sent");
        assert!(rig.driver.in_pause);
    }

    #[test]
    fn heartbeat_pauses_on_an_idle_listener() {
        // A listener of 'none', 'emulator' or 'test' → PAUSE with the last frame, even with a fresh
        // live frame.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        rig.activity.update(|a| a.game_running = "dirt".into());
        rig.call(|d, ctx| d.send_stream(&[1000; 8], ctx)).unwrap();
        let link = rig.link.clone();
        rig.driver.heartbeat_tick(&link, true, &rig.host.clock);
        assert_eq!(position_frames(&rig).last().unwrap(), &position_frame(RunState::Pause, [1000; 8]).to_vec());
        // Motion-test mode and the motion switch suppress the heartbeat.
        let sent = position_frames(&rig).len();
        rig.driver.heartbeat_tick(&link, false, &rig.host.clock);
        rig.activity.update(|a| a.motion_test_mode = true);
        rig.driver.heartbeat_tick(&link, true, &rig.host.clock);
        assert_eq!(position_frames(&rig).len(), sent);
        assert_eq!(rig.driver.heartbeat_interval(), openrigcue_model::python_time::sleep_duration(1.0 / 30.0).unwrap());
    }

    /// The heartbeat rate: rate ≤ 0 → 30; the 30 Hz cap only off Windows;
    /// on Windows the configured rate is used.
    #[test]
    fn heartbeat_interval_caps_only_off_windows() {
        let interval = |refresh_rate, windows| {
            let settings = ActuonSettings { refresh_rate, windows, ..ActuonSettings::default() };
            ActuonDriver::new(LiveSettings::new(settings), LiveSettings::new(HostActivity::default()))
                .heartbeat_interval()
        };
        assert_eq!(interval(100, true), openrigcue_model::python_time::sleep_duration(1.0 / 100.0).unwrap());
        assert_eq!(interval(100, false), openrigcue_model::python_time::sleep_duration(1.0 / 30.0).unwrap());
        assert_eq!(interval(20, false), openrigcue_model::python_time::sleep_duration(1.0 / 20.0).unwrap());
        assert_eq!(interval(0, true), openrigcue_model::python_time::sleep_duration(1.0 / 30.0).unwrap());
        assert_eq!(interval(-5, false), openrigcue_model::python_time::sleep_duration(1.0 / 30.0).unwrap());
    }

    #[test]
    fn heartbeat_interval_uses_native_sleep_rounding() {
        let settings = ActuonSettings { refresh_rate: 90, windows: true, ..ActuonSettings::default() };
        let driver = ActuonDriver::new(LiveSettings::new(settings), LiveSettings::new(HostActivity::default()));
        let expected = if cfg!(windows) { 11_111_200 } else { 11_111_112 };
        assert_eq!(driver.heartbeat_interval(), Duration::from_nanos(expected));
    }

    #[test]
    fn heartbeat_pauses_a_live_game_after_100_ms_without_frames() {
        // More than 0.1 s since the last live frame also pauses while a live listener runs.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        rig.activity.update(|a| {
            a.game_running = "dirt".into();
            a.listener_running = "dirt".into();
        });
        rig.call(|d, ctx| d.send_stream(&[1000; 8], ctx)).unwrap();
        let link = rig.link.clone();
        let sent = position_frames(&rig).len();
        rig.host.clock.sleep(Duration::from_millis(100));
        rig.driver.heartbeat_tick(&link, true, &rig.host.clock);
        assert_eq!(position_frames(&rig).len(), sent, "exactly 0.1 s is not stale");
        rig.host.clock.sleep(Duration::from_millis(1));
        rig.driver.heartbeat_tick(&link, true, &rig.host.clock);
        assert_eq!(position_frames(&rig).last().unwrap(), &position_frame(RunState::Pause, [1000; 8]).to_vec());
        // The next live frame resumes.
        rig.call(|d, ctx| d.send_stream(&[1000; 8], ctx)).unwrap();
        assert_eq!(position_frames(&rig).last().unwrap()[2], RunState::Resume as u8);
    }

    #[test]
    fn shared_handle_drives_the_registry_driver() {
        // The heartbeat starts on connect: the heartbeat thread and the stream frames share the driver state.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        let driver =
            std::mem::replace(&mut rig.driver, ActuonDriver::new(LiveSettings::default(), rig.activity.clone()));
        driver.settings.update(|s| s.motion_switch_on = true);
        type Bodies = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;
        let started = Bodies::default();
        let kept = Arc::clone(&started);
        let shared =
            SharedActuon::with_heartbeat_spawner(driver, Box::new(move |body| kept.lock().unwrap().push(body)));
        let mut registered = (shared.factory())();
        let link = SharedLink::default();
        registered.attach_link(link.clone());
        let spec = motion_model("4 Actuators [2 Front + 2 Rear]", false).unwrap();
        let mut ctx = MotionContext { model: spec, link: &mut rig.link, host: &mut rig.host };
        registered.connect(&mut ctx).unwrap();
        registered.connect(&mut ctx).unwrap();
        assert_eq!(started.lock().unwrap().len(), 1, "one heartbeat thread for the process");
        link.update(|l| *l = rig.link.clone());
        rig.activity.update(|a| a.game_running = "dirt".into());
        shared.heartbeat_pass(&rig.host.clock);
        assert_eq!(position_frames(&rig).last().unwrap(), &position_frame(RunState::Pause, [PARK; 8]).to_vec());
        assert_eq!(shared.connected_port().as_deref(), Some("/dev/ttyUSB2"));
    }

    #[test]
    fn failed_manual_port_runs_the_not_connected_tail() {
        // Connect: a port that connects ends it; else the first-scan flag is cleared,
        // the motion status is 'Not connected' and the motion ports are empty.
        let mut s = cached("/dev/ttyUSB2");
        s.port = "/dev/ttyUSB7".into();
        let mut rig = Rig::new("Actuon8(M)", s);
        rig.board.fail_open.lock().unwrap().push("/dev/ttyUSB7".into());
        rig.link.status = LinkStatus::Connected;
        rig.link.ports.insert(0, "/dev/ttyUSB7".into());
        rig.call(|d, ctx| d.connect(ctx)).unwrap();
        assert!(!rig.link.is_connected());
        assert!(rig.link.ports.is_empty());
        assert!(!rig.driver.first_scan);
    }

    #[test]
    fn reconnect_with_unparsable_get56_keeps_the_config_and_empty_reply_clears_it() {
        // The handshake: a configuration that cannot be parsed leaves the stored one
        // and rejects the board. A non-CAP/CFG reply stores an empty map, which stops the
        // spike-filter push.
        let mut s = cached("/dev/ttyUSB2");
        s.spike_filter_vertical = 1800;
        let mut rig = Rig::new("Actuon8(M)", s);
        rig.connect();
        assert_eq!(rig.driver.config().len(), 17);
        *rig.board.config_line.lock().unwrap() = "CFG:56:x:1:2".into();
        rig.connect();
        assert!(!rig.link.is_connected(), "is_connected returned False");
        assert_eq!(rig.driver.config().len(), 17, "unchanged after a parse error");
        *rig.board.config_line.lock().unwrap() = "ERR:56".into();
        rig.connect();
        assert!(rig.link.is_connected());
        assert!(rig.driver.config().is_empty());
        rig.call(|d, ctx| d.send_stream(&[1; 8], ctx)).unwrap();
        assert!(!rig.written().iter().any(|c| c.starts_with(b"SET")));
    }

    #[test]
    fn leaving_test_mode_parks_the_platform() {
        // Test frames: test mode 1 → 0 while no game runs → the stop sends PAUSE with 32768 × 8.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        rig.activity.update(|a| a.motion_test_mode = true);
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        rig.activity.update(|a| a.motion_test_mode = false);
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        let frames = position_frames(&rig);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], position_frame(RunState::Pause, [PARK; 8]).to_vec());
    }

    #[test]
    fn send_test_runs_only_in_test_mode_with_game_none_or_test() {
        // Test frames: test mode on and a game key of 'none' or 'test' → a motion frame.
        let mut rig = Rig::new("Actuon8(M)", cached("/dev/ttyUSB2"));
        rig.connect();
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        assert!(position_frames(&rig).is_empty(), "no test mode: nothing sent");
        rig.activity.update(|a| {
            a.motion_test_mode = true;
            a.game_running = "test".into();
        });
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        assert_eq!(position_frames(&rig).len(), 1);
        rig.activity.update(|a| a.game_running = "dirt".into());
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        assert_eq!(position_frames(&rig).len(), 1, "a running game drops test frames");
        // Leaving test mode while a game runs does not park.
        rig.activity.update(|a| a.motion_test_mode = false);
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        assert_eq!(position_frames(&rig).len(), 1);
        // Leaving test mode with the game key 'test' parks.
        rig.activity.update(|a| {
            a.motion_test_mode = true;
            a.game_running = "test".into();
        });
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        rig.activity.update(|a| a.motion_test_mode = false);
        rig.call(|d, ctx| d.send_test(&[1000; 8], ctx)).unwrap();
        assert_eq!(position_frames(&rig).last().unwrap(), &position_frame(RunState::Pause, [PARK; 8]).to_vec());
    }

    /// The level of a raw tune value: an integer of any width, clamped to 0..=65535 before rounding.
    #[test]
    fn native_to_kg_clamps_wide_and_text_values() {
        let wide = LegacyValue::from_integer(num_bigint::BigInt::from(1) << 90);
        assert_eq!(native_to_kg(&wide).unwrap(), 254);
        assert_eq!(native_to_kg(&LegacyValue::from_integer(-(num_bigint::BigInt::from(1) << 90usize))).unwrap(), 1);
        assert_eq!(native_to_kg(&LegacyValue::from("1800")).unwrap(), 8);
        assert_eq!(native_to_kg(&LegacyValue::Float(1800.9)).unwrap(), 8);
        assert_eq!(
            native_to_kg(&LegacyValue::from("x")).unwrap_err().exception_line().unwrap(),
            "ValueError: invalid literal for int() with base 10: 'x'"
        );
    }
}

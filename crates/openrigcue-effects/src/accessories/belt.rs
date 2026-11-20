//! SRS belt tensioner: an ESP32 CmdMessenger bridge to two Dayond servo drives on CAN, tightening the
//! seat belts from surge, sway and heave.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use openrigcue_model::native_value::LegacyValue;
use tracing::debug;

use super::cmd_messenger::{self, Arg, CmdError, CommandSpec, Field, Message};
use super::link::{is_port_gone, Clock, PortEntry, PortLister, PortOpener, PortScanError, PortSettings, SerialLink};
use super::numeric::{map_clamped, truncate, PyIntError};
use super::open_gate::ask_identity;
use super::port_scan::{AllowedPorts, ClaimedPorts, Device, Exclusions, ScanRules};
use super::status::DeviceStatus;

use Field::{Byte as B, Text as S};

/// Command table of the ESP32 bridge.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "who_are_you", fields: &[S] },
    CommandSpec { name: "i_am", fields: &[S, B, B, B, B, S] },
    CommandSpec { name: "send_message", fields: &[B, B, B, B, B, B, B, B, B] },
    CommandSpec { name: "received_std_message", fields: &[S, B, B, B, B, B, B, B, B, B] },
    CommandSpec { name: "received_remote_message", fields: &[S, B] },
    CommandSpec { name: "logger", fields: &[S] },
    CommandSpec { name: "gen_hash", fields: &[] },
    // Firmware command slot 7 (its firmware name); OpenRigCue never sends it.
    CommandSpec { name: "write_license_key", fields: &[S] },
    CommandSpec { name: "error", fields: &[S] },
];

const WHO_ARE_YOU: usize = 0;
const I_AM: usize = 1;
const SEND_MESSAGE: usize = 2;
const RECEIVED_STD_MESSAGE: usize = 3;

/// The belt firmware version this build expects.
pub const SHIPPED_FIRMWARE: i64 = 5;
/// `i_am` product byte of the belt.
const BELT_PRODUCT: i64 = 1;

/// A servo drive on the CAN bus: arbitration id and the group id sent as first data byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drive {
    pub arbitration_id: u8,
    pub group_id: u8,
}

/// Left drive (arbitration id 1, group id 0).
pub const LEFT: Drive = Drive { arbitration_id: 1, group_id: 0 };
/// Right drive (arbitration id 2, group id 1).
pub const RIGHT: Drive = Drive { arbitration_id: 2, group_id: 1 };

const WRITE_COMMAND: u8 = 0x1A;
const READ_COMMAND: u8 = 0x2A;

/// Dayond registers used by the belt.
mod reg {
    pub const ENABLE: u8 = 0x00;
    pub const MODE: u8 = 0x02;
    pub const TORQUE: u8 = 0x08;
    pub const POSITION_RAMP: u8 = 0x09;
    pub const POSITION_MODE_SPEED: u8 = 0x1D;
    pub const POSITION_LOW: u8 = 0x05;
    pub const CLEAR_FAULT_A: u8 = 0x4A;
    pub const CLEAR_FAULT_B: u8 = 0x4C;
    pub const DISABLE_COMPANION: u8 = 0x4F;
    pub const POSITION_HIGH: u8 = 0x50;
    pub const POSITION_RELATIVE: u8 = 0x51;
    pub const BUS_VOLTAGE: u8 = 0xE1;
    pub const OUTPUT_CURRENT: u8 = 0xE2;
    pub const STATUS: u8 = 0xE3;
    pub const OUTPUT_SPEED: u8 = 0xE4;
}

const MODE_TORQUE: u8 = 0xC1;
const MODE_POSITION: u8 = 0xD0;

/// The value wrapped to 16 bits, as `[hi, lo]`.
fn word_bytes(value: i64) -> [u8; 2] {
    (value as u16).to_be_bytes()
}

/// Register values reported by the drives, keyed by arbitration id then register.
#[derive(Debug, Default)]
struct BusState {
    registers: Mutex<HashMap<i64, HashMap<u8, u16>>>,
    stop: AtomicBool,
    /// Set when the receive thread closes the board: the board takes no more frames.
    closed: AtomicBool,
}

impl BusState {
    fn register(&self, drive: Drive, register: u8) -> Option<u16> {
        self.registers.lock().unwrap().get(&i64::from(drive.arbitration_id)).and_then(|r| r.get(&register)).copied()
    }

    /// Store the two register values of a `received_std_message` (`[arb_id, _, grp, cmd, a1, h1, l1, a2,
    /// h2, l2]`): the first field is the arbitration id, the payload starts at the third. `None` for a malformed frame (a
    /// message without arguments, an arbitration id that is not an integer), which the receive loop skips.
    fn ingest(&self, msg: &Message) -> Option<()> {
        let arbitration = cmd_messenger::python_int_arg(msg.args.first()?);
        if arbitration == Err(PyIntError::Invalid) {
            return None;
        }
        // The table gives this command ten typed fields, so a message with arguments has eight payload bytes.
        let payload: Vec<i64> = msg.args.iter().skip(2).map(|v| v.as_int()).collect::<Option<_>>()?;
        let byte = |i: usize| payload.get(i).copied();
        let (a1, h1, l1, a2, h2, l2) = (byte(2)?, byte(3)?, byte(4)?, byte(5)?, byte(6)?, byte(7)?);
        // An id beyond 64 bits is stored under a key no drive is looked up with.
        let Ok(arbitration) = arbitration else { return Some(()) };
        let mut registers = self.registers.lock().unwrap();
        let drive = registers.entry(arbitration).or_default();
        drive.insert(a1 as u8, ((h1 << 8) | l1) as u16);
        drive.insert(a2 as u8, ((h2 << 8) | l2) as u16);
        Some(())
    }
}

/// Seconds without a CAN frame after which a connected belt is declared lost.
const WATCHDOG: Duration = Duration::from_secs(3);

/// The belt status and held port as the receive thread's watchdog writes them.
pub trait BeltWatchdog: Send + Sync {
    /// A "Connected" belt becomes "Not Connected" with no port;
    /// true when the belt was connected. Gives up with false once `stop` is set, so whoever stops
    /// the receive thread never waits for it.
    fn disconnect_if_connected(&self, stop: &AtomicBool) -> bool;
}

/// Run state of a drive (the run bit of the status word, or no answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Stop,
    Start,
    NoResponse,
}

/// Decoded status word (register 0xE3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveStatus {
    pub running: bool,
    pub overcurrent: bool,
    pub overvoltage: bool,
    pub encoder_failure: bool,
    pub position_deviation: bool,
    pub undervoltage: bool,
    pub overload: bool,
    /// "External control" is decoded from bit 6 as well (same bit as overload).
    pub external_control: bool,
}

impl DriveStatus {
    /// Decodes the bits of a status word.
    pub fn from_word(word: u16) -> Self {
        let bit = |n: u16| (word >> n) & 1 == 1;
        DriveStatus {
            running: bit(0),
            overcurrent: bit(1),
            overvoltage: bit(2),
            encoder_failure: bit(3),
            position_deviation: bit(4),
            undervoltage: bit(5),
            overload: bit(6),
            external_control: bit(6),
        }
    }
}

/// The Dayond register protocol over the ESP32 bridge.
///
/// Frames address two 16-bit registers each
/// (`[group, 0x1A|0x2A, a1, hi1, lo1, a2, hi2, lo2]`), single register writes/reads are queued and only
/// leave in pairs, and a receive thread caches every register value the drives report.
pub struct DayondBus {
    link: Box<dyn SerialLink>,
    pending_writes: Vec<u8>,
    pending_reads: Vec<u8>,
    /// The latest speed, acceleration time and deceleration time (defaults 10, 1, 1).
    latest: (f64, i64, i64),
    state: Arc<BusState>,
    clock: Arc<dyn Clock>,
    receiver: Option<JoinHandle<()>>,
}

impl DayondBus {
    /// Start the protocol and its receive thread on `link`.
    /// `watchdog` is where the receive thread marks the belt "Not Connected" after 3 s without a frame.
    pub fn start(
        link: Box<dyn SerialLink>,
        clock: Arc<dyn Clock>,
        watchdog: Arc<dyn BeltWatchdog>,
    ) -> std::io::Result<Self> {
        let state = Arc::new(BusState::default());
        let mut reader = link.try_clone()?;
        let rx_state = Arc::clone(&state);
        let rx_clock = Arc::clone(&clock);
        let receiver = std::thread::Builder::new()
            .name("belt_recv_thread".into())
            .spawn(move || receive_loop(reader.as_mut(), &rx_state, rx_clock.as_ref(), watchdog.as_ref()))?;
        Ok(DayondBus {
            link,
            pending_writes: Vec::new(),
            pending_reads: Vec::new(),
            latest: (10.0, 1, 1),
            state,
            clock,
            receiver: Some(receiver),
        })
    }

    /// Stop the receive thread; from the receive thread itself (its watchdog releasing the bus) the
    /// thread only stops after the current pass.
    pub fn stop(&mut self) {
        self.state.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.receiver.take() {
            if handle.thread().id() != std::thread::current().id() {
                let _ = handle.join();
            }
        }
    }

    fn frame(&mut self, drive: Drive, command: u8, registers: &[u8]) -> Result<(), CmdError> {
        if self.state.closed.load(Ordering::SeqCst) {
            // The error text for a closed port: "Attempting to use a port that is not open".
            return Err(CmdError::Io(std::io::Error::new(std::io::ErrorKind::NotConnected, "port is closed")));
        }
        let mut args = vec![
            Arg::Number(drive.arbitration_id.into()),
            Arg::Number(drive.group_id.into()),
            Arg::Number(command.into()),
        ];
        args.extend(registers.iter().map(|b| Arg::Number(f64::from(*b))));
        cmd_messenger::send(self.link.as_mut(), COMMANDS, SEND_MESSAGE, &args)
    }

    /// Two register writes in one frame, bypassing the queue.
    fn write_pair(&mut self, drive: Drive, first: [u8; 3], second: [u8; 3]) -> Result<(), CmdError> {
        self.frame(drive, WRITE_COMMAND, &[first, second].concat())
    }

    /// Queue one register write; a frame leaves (with this call's drive) once two are queued.
    fn queue_write(&mut self, drive: Drive, register: [u8; 3]) -> Result<(), CmdError> {
        self.pending_writes.extend_from_slice(&register);
        if self.pending_writes.len() >= 6 {
            let pair: Vec<u8> = self.pending_writes.drain(..6).collect();
            self.frame(drive, WRITE_COMMAND, &pair)?;
        }
        Ok(())
    }

    /// Queue one register read.
    fn queue_read(&mut self, drive: Drive, register: [u8; 3]) -> Result<(), CmdError> {
        self.pending_reads.extend_from_slice(&register);
        if self.pending_reads.len() >= 6 {
            let pair: Vec<u8> = self.pending_reads.drain(..6).collect();
            self.frame(drive, READ_COMMAND, &pair)?;
        }
        Ok(())
    }

    /// Turn the motor on.
    pub fn turn_on(&mut self, drive: Drive) -> Result<(), CmdError> {
        self.write_pair(drive, [reg::ENABLE, 0, 1], [reg::ENABLE, 0, 1])
    }

    /// Turn the motor off.
    pub fn turn_off(&mut self, drive: Drive) -> Result<(), CmdError> {
        self.write_pair(drive, [reg::DISABLE_COMPANION, 0, 0], [reg::ENABLE, 0, 0])
    }

    /// Select torque mode.
    pub fn torque_mode(&mut self, drive: Drive) -> Result<(), CmdError> {
        self.queue_write(drive, [reg::MODE, 0, MODE_TORQUE])?;
        self.queue_write(drive, [reg::MODE, 0, MODE_TORQUE])
    }

    /// Select position mode (absolute positions) with a speed and acceleration/deceleration times.
    pub fn position_mode(
        &mut self,
        drive: Drive,
        speed: f64,
        acceleration: i64,
        deceleration: i64,
    ) -> Result<(), CmdError> {
        self.latest = (speed, acceleration, deceleration);
        self.write_pair(
            drive,
            [reg::MODE, 0, MODE_POSITION],
            [reg::POSITION_RAMP, (acceleration & 0xFF) as u8, (deceleration & 0xFF) as u8],
        )?;
        self.queue_write(drive, [reg::POSITION_RELATIVE, 0, 0])?;
        let [hi, lo] = word_bytes((speed * 8192.0 / 3000.0) as i64);
        self.queue_write(drive, [reg::POSITION_MODE_SPEED, hi, lo])
    }

    /// Set the torque: register 0x08 written twice (one frame when the queue was empty).
    pub fn set_torque(&mut self, drive: Drive, torque: i64) -> Result<(), CmdError> {
        let [hi, lo] = word_bytes(torque);
        self.queue_write(drive, [reg::TORQUE, hi, lo])?;
        self.queue_write(drive, [reg::TORQUE, hi, lo])
    }

    /// Set the position: signed 32-bit steps, high word to 0x50 and low word to 0x05.
    pub fn set_position(&mut self, drive: Drive, position: i64) -> Result<(), CmdError> {
        let b = (position as i32).to_be_bytes();
        self.write_pair(drive, [reg::POSITION_HIGH, b[0], b[1]], [reg::POSITION_LOW, b[2], b[3]])
    }

    /// Clear the drive's fault.
    pub fn clear_fault(&mut self, drive: Drive) -> Result<(), CmdError> {
        self.queue_write(drive, [reg::CLEAR_FAULT_A, 0, 0])?;
        self.queue_write(drive, [reg::CLEAR_FAULT_B, 0, 0])
    }

    /// Output current in amperes (raw / 100), -1 when never reported.
    pub fn output_current(&self, drive: Drive) -> f64 {
        self.state.register(drive, reg::OUTPUT_CURRENT).map_or(-1.0, |v| f64::from(v) / 100.0)
    }

    /// Output speed in rpm (raw * 6000 / 16384), -1 when never reported.
    pub fn output_speed(&self, drive: Drive) -> f64 {
        self.state.register(drive, reg::OUTPUT_SPEED).map_or(-1.0, |v| f64::from(v) * 6000.0 / 16384.0)
    }

    /// Bus voltage and status word: request 0xE1/0xE3 and wait (1 ms polls) until
    /// both are cached; `None` on timeout.
    pub fn bus_status(&mut self, drive: Drive, timeout: Duration) -> Result<Option<(u16, DriveStatus)>, CmdError> {
        self.queue_read(drive, [reg::BUS_VOLTAGE, 0, 0])?;
        self.queue_read(drive, [reg::STATUS, 0, 0])?;
        // The timeout is measured on the wall clock from the start of the wait.
        let start = self.clock.time();
        while self.clock.time() - start < timeout.as_secs_f64() {
            if let (Some(voltage), Some(word)) =
                (self.state.register(drive, reg::BUS_VOLTAGE), self.state.register(drive, reg::STATUS))
            {
                return Ok(Some((voltage, DriveStatus::from_word(word))));
            }
            self.clock.sleep(Duration::from_millis(1));
        }
        debug!("DayondServoProtocol - get_bus_voltage_and_fault_state failed");
        Ok(None)
    }
}

impl Drop for DayondBus {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The receive thread: cache register frames; after 3 s without any message, a belt whose status is "Connected"
/// becomes "Not Connected" with no port, the board is closed and the thread stops
/// (while the belt is not "Connected" the loop goes on). A receive error of a port whose device is
/// gone or that is closed (see [`is_port_gone`]) closes the board and
/// ends the thread without touching the belt's status or port; every other receive error is printed and the
/// loop goes on.
fn receive_loop(link: &mut dyn SerialLink, state: &BusState, clock: &dyn Clock, watchdog: &dyn BeltWatchdog) {
    // The silence is measured on the wall clock since the last message.
    let mut last_frame = clock.time();
    while !state.stop.load(Ordering::SeqCst) {
        if clock.time() - last_frame > WATCHDOG.as_secs_f64() && watchdog.disconnect_if_connected(&state.stop) {
            state.closed.store(true, Ordering::SeqCst);
            state.stop.store(true, Ordering::SeqCst);
            return;
        }
        match cmd_messenger::receive(link, COMMANDS) {
            Ok(Some(msg)) if msg.is(RECEIVED_STD_MESSAGE) => {
                // A malformed frame stores nothing; it still counts as traffic for the watchdog.
                if state.ingest(&msg).is_none() {
                    debug!("belt receive thread skipped a malformed received_std_message");
                }
                last_frame = clock.time();
            }
            // Every other message the board sends also counts as traffic for the watchdog.
            Ok(Some(_)) => last_frame = clock.time(),
            Ok(None) => {}
            Err(CmdError::Io(e)) if is_port_gone(&e) => {
                debug!("belt receive thread ended: {e}");
                state.closed.store(true, Ordering::SeqCst);
                state.stop.store(true, Ordering::SeqCst);
                return;
            }
            Err(e) => debug!("error receiveing callback repsonse {e}"),
        }
    }
}

/// Belt tuning of the game profile (the `belt_*` tune keys).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeltTuning {
    pub surge_enabled: bool,
    pub surge_reverse: bool,
    /// `belt_surge` (1..20).
    pub surge: f64,
    /// `belt_surge_smoothing` (0..100).
    pub surge_smoothing: i64,
    pub heave_enabled: bool,
    pub heave_factor: f64,
    pub heave_smoothing: i64,
    pub sway_enabled: bool,
    pub sway_reverse: bool,
    pub sway_factor: f64,
    pub sway_smoothing: i64,
}

/// `BELT_TENSIONER` calibration settings (0..100 sliders).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeltCalibration {
    pub power: i64,
    pub retraction: i64,
}

/// Normalisation maxima (`max_telemetry_vars`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeltMaxima {
    pub surge_acceleration: f64,
    pub surge_deceleration: f64,
    pub heave: f64,
    pub sway: f64,
}

/// `belt_homing`: calibration state shared with the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Homing {
    /// 0: not calibrated.
    NotCalibrated,
    /// 1: calibrated, streaming.
    Calibrated,
    /// 2: calibration requested.
    Requested,
    /// 3: calibration running.
    InProgress,
}

/// The controller's homing parameter, readable and writable while its device mutex is busy.
#[derive(Debug, Default, Clone)]
pub(super) struct PublishedHoming(Arc<AtomicU8>);

impl PublishedHoming {
    pub(super) fn get(&self) -> Homing {
        match self.0.load(Ordering::SeqCst) {
            1 => Homing::Calibrated,
            2 => Homing::Requested,
            3 => Homing::InProgress,
            _ => Homing::NotCalibrated,
        }
    }

    pub(super) fn set(&self, value: Homing) {
        self.0.store(value as u8, Ordering::SeqCst);
    }
}

/// Drive control mode (`belt_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeltMode {
    Torque,
    Position,
}

/// The runtime switches the belt reads and writes, and the number of belt motors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeltSwitches {
    pub on: bool,
    pub homing: Homing,
    pub test_mode: bool,
    /// `belt_test_value` (0..100).
    pub test_value: f64,
    pub motors: u8,
    pub mode: BeltMode,
}

impl Default for BeltSwitches {
    fn default() -> Self {
        BeltSwitches {
            on: false,
            homing: Homing::NotCalibrated,
            test_mode: false,
            test_value: 0.0,
            motors: 2,
            mode: BeltMode::Torque,
        }
    }
}

/// Torque used for homing.
pub const HOMING_TORQUE: i64 = -400;
/// Position mode parameters: speed, acceleration and deceleration.
const POSITION_MODE_RPM: f64 = 3000.0;
const POSITION_MODE_RAMP: i64 = 5;
/// Full-scale belt travel in steps.
const MAX_STEPS: f64 = 100_000.0;
/// Retraction offset range.
const OFFSET_MIN: f64 = 20_000.0;
const OFFSET_MAX: f64 = 80_000.0;
/// Fully slack positions sent when the belt is switched off in position mode.
const SLACK_POSITION: i64 = 200_000;
/// Moving-average history length.
const HISTORY: usize = 25;

/// Effect fraction: only negative input pulls (a positive value gives 0, anything else its magnitude, so a
/// NaN stays NaN), full-scale telemetry -> 0.5, times `slider / 10`.
pub fn effect_fraction(effect: f64, max_telemetry: f64, slider: f64) -> f64 {
    let effect = if effect > 0.0 { 0.0 } else { effect.abs() };
    let perc = if max_telemetry != 0.0 { effect / (max_telemetry * 2.0).abs() } else { 0.0 };
    perc * (slider / 10.0)
}

/// Which belt a sway fraction is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Sway fraction: the left belt reacts to negative sway, the right one to positive sway.
pub fn sway_fraction(side: Side, sway: f64, max_sway: f64, slider: f64) -> f64 {
    if (side == Side::Left && sway > 0.0) || (side == Side::Right && sway < 0.0) || sway == 0.0 {
        return 0.0;
    }
    let perc = if max_sway != 0.0 { sway / (max_sway * 2.0).abs() } else { 0.0 };
    (perc * (slider / 10.0)).abs()
}

/// Moving average: the average of the current value and the last
/// `smoothing / 5` (truncated) values. The loop over the history, newest first, returns the current value if it exhausts the history;
/// zero/negative windows still reach its division when the history is nonempty.
pub fn moving_average(current: f64, history: &VecDeque<f64>, smoothing: i64) -> Result<f64, BeltError> {
    let window = (smoothing as f64 / 5.0).trunc() as i64;
    if history.is_empty() || (history.len() as i64) < window {
        return Ok(current);
    }
    let mut total = 0.0;
    for (index, value) in history.iter().rev().enumerate() {
        if index as i64 >= window {
            let divisor = (window + 1) as f64;
            if divisor == 0.0 {
                return Err(BeltError::DivisionByZero);
            }
            return Ok((total + current) / divisor);
        }
        total += value;
    }
    Ok(current)
}

/// Appends to a history capped at 25 values, dropping the oldest.
fn push_history(history: &mut VecDeque<f64>, value: f64) {
    history.push_back(value);
    while history.len() > HISTORY {
        history.pop_front();
    }
}

/// One smoothed channel: last value and its history.
#[derive(Debug, Clone, Default)]
struct Smoothed {
    previous: f64,
    history: VecDeque<f64>,
}

impl Smoothed {
    /// The smoothed value; `None` (after logging) when the average cannot be computed, and the caller then
    /// sends nothing this frame.
    fn smooth_native(
        &self,
        value: f64,
        compare_with: f64,
        profile: Option<&LegacyValue>,
        key: &str,
        fallback: i64,
        label: &str,
    ) -> Option<f64> {
        if value == compare_with {
            return Some(value);
        }
        let result = (|| -> Result<f64, super::hapticr::NativeEngineError> {
            use super::values as v;
            let smoothing = profile.map_or(Ok(LegacyValue::Int(fallback)), |p| v::item(p, key))?;
            let window = LegacyValue::from_integer(
                smoothing.py_truediv(&LegacyValue::Int(5)).and_then(|v| v.py_integer()).map_err(v::operation)?,
            );
            if self.history.is_empty()
                || LegacyValue::Int(self.history.len() as i64).py_lt(&window).map_err(v::operation)?
            {
                return Ok(value);
            }
            let mut sum = LegacyValue::Int(0);
            for (index, previous) in self.history.iter().rev().enumerate() {
                if LegacyValue::Int(index as i64).py_ge(&window).map_err(v::operation)? {
                    let denominator = window.py_add(&LegacyValue::Int(1)).map_err(v::operation)?;
                    return sum
                        .py_add(&LegacyValue::Float(value))
                        .and_then(|v| v.py_truediv(&denominator))
                        .and_then(|v| v.py_as_double())
                        .map_err(v::operation);
                }
                sum = sum.py_add(&LegacyValue::Float(*previous)).map_err(v::operation)?;
            }
            Ok(value)
        })();
        if result.is_err() {
            // Only the literal "failed to calculate <name>" is logged.
            debug!("failed to calculate {label}");
        }
        result.ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BeltError {
    #[error(transparent)]
    Bus(#[from] CmdError),
    #[error("float division by zero")]
    DivisionByZero,
    /// Integer conversion failure after mapping.
    #[error("{0} is not a finite number")]
    NotFinite(&'static str),
    #[error("TypeError: must be real number, not {0}")]
    InvalidTestValue(&'static str),
    #[error(transparent)]
    Native(#[from] super::hapticr::NativeEngineError),
}

/// Belt control state.
#[derive(Debug, Default)]
pub struct BeltState {
    surge: Smoothed,
    heave: Smoothed,
    sway_left: Smoothed,
    sway_right: Smoothed,
    homing_status: Option<PublishedHoming>,
    native_test_value: Option<(LegacyValue, u64)>,
}

/// Retraction offset in steps for the calibration slider.
#[cfg(test)]
fn retraction_offset(retraction: i64) -> i64 {
    -truncate(map_clamped(retraction as f64, 0.0, 100.0, OFFSET_MIN, OFFSET_MAX)).unwrap_or(0)
}

fn native_retraction_offset(settings: Option<&LegacyValue>, fallback: i64) -> Result<i64, BeltError> {
    use super::values as v;
    let value = settings.map_or(Ok(LegacyValue::Int(fallback)), |settings| {
        v::field(settings, "BELT_TENSIONER", "belt_calibration_retraction")
    })?;
    let value =
        LegacyValue::from_integer(value.py_integer().map_err(v::operation)?).py_as_double().map_err(v::operation)?;
    Ok(-truncate(map_clamped(value, 0.0, 100.0, OFFSET_MIN, OFFSET_MAX)).ok_or(BeltError::NotFinite("belt offset"))?)
}

fn native_effect_fraction(
    effect: &LegacyValue,
    maximum: &LegacyValue,
    slider: &LegacyValue,
    side: Option<Side>,
) -> Result<f64, BeltError> {
    use super::values as v;
    let zero = LegacyValue::Int(0);
    let effect = if let Some(side) = side {
        if (side == Side::Left && effect.py_gt(&zero).map_err(v::operation)?)
            || (side == Side::Right && effect.py_lt(&zero).map_err(v::operation)?)
            || openrigcue_model::python_compare::native_equal(effect, &zero)
        {
            return Ok(0.0);
        }
        effect.clone()
    } else if effect.py_gt(&zero).map_err(v::operation)? {
        zero.clone()
    } else {
        effect.py_abs().map_err(v::operation)?
    };
    let fraction = if openrigcue_model::python_compare::native_equal(maximum, &zero) {
        zero
    } else {
        let denominator = maximum.py_mul(&LegacyValue::Int(2)).and_then(|v| v.py_abs()).map_err(v::operation)?;
        effect.py_truediv(&denominator).map_err(v::operation)?
    };
    let scale = slider.py_truediv(&LegacyValue::Int(10)).map_err(v::operation)?;
    let result = fraction.py_mul(&scale).map_err(v::operation)?;
    let result = if side.is_some() { result.py_abs().map_err(v::operation)? } else { result };
    result.py_as_double().map_err(v::operation).map_err(Into::into)
}

/// Belt target in steps for an effect fraction: 0..1.3 mapped to 0..100000, truncated and negated.
/// The shared rescale clamps before the integer conversion, including non-finite inputs.
fn target_steps(fraction: f64) -> Result<i64, BeltError> {
    truncate(map_clamped(fraction, 0.0, 1.3, 0.0, MAX_STEPS))
        .map(|steps| -steps)
        .ok_or(BeltError::NotFinite("belt position"))
}

/// What one telemetry frame feeds the belt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeltInputs {
    /// `longitudinal_acceleration`.
    pub surge: f64,
    /// `lateral_acceleration`.
    pub sway: f64,
    /// `vertical_acceleration`.
    pub heave: f64,
}

impl BeltState {
    pub fn assign_native_test_value(&mut self, value: LegacyValue, projection: f64) {
        self.native_test_value = Some((value, projection.to_bits()));
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn with_homing_status(homing_status: PublishedHoming) -> Self {
        Self { homing_status: Some(homing_status), ..Self::default() }
    }

    fn set_homing(&self, switches: &mut BeltSwitches, value: Homing) {
        switches.homing = value;
        if let Some(status) = &self.homing_status {
            status.set(value);
        }
    }

    fn homing(&self, switches: &mut BeltSwitches) -> Homing {
        if let Some(status) = &self.homing_status {
            switches.homing = status.get();
        }
        switches.homing
    }

    /// Drive the belts for one frame.
    ///
    /// Switch handling (slacken and return to torque mode
    /// when off), homing on request, test positions, re-entering position mode, then the streamed positions:
    /// the negated step target of `surge + heave + sway_side`, minus the retraction (mirrored for the right drive).
    #[allow(clippy::too_many_arguments)]
    pub fn send_stream(
        &mut self,
        bus: &mut DayondBus,
        switches: &mut BeltSwitches,
        inputs: BeltInputs,
        tuning: &BeltTuning,
        maxima: &BeltMaxima,
        calibration: &BeltCalibration,
    ) -> Result<(), BeltError> {
        self.send_stream_values(bus, switches, inputs, tuning, maxima, calibration, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_stream_for_telemetry(
        &mut self,
        bus: &mut DayondBus,
        switches: &mut BeltSwitches,
        inputs: BeltInputs,
        tuning: &BeltTuning,
        maxima: &BeltMaxima,
        calibration: &BeltCalibration,
        telemetry: &openrigcue_model::Telemetry,
        profile: Option<&LegacyValue>,
        settings: Option<&LegacyValue>,
    ) -> Result<(), BeltError> {
        self.send_stream_values(bus, switches, inputs, tuning, maxima, calibration, Some(telemetry), profile, settings)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_stream_values(
        &mut self,
        bus: &mut DayondBus,
        switches: &mut BeltSwitches,
        inputs: BeltInputs,
        tuning: &BeltTuning,
        maxima: &BeltMaxima,
        calibration: &BeltCalibration,
        telemetry: Option<&openrigcue_model::Telemetry>,
        profile: Option<&LegacyValue>,
        settings: Option<&LegacyValue>,
    ) -> Result<(), BeltError> {
        use super::values as v;
        let raw =
            |key, projection| telemetry.map_or(LegacyValue::Float(projection), |t| v::channel(t, key, projection));
        let mut surge = raw("longitudinal_acceleration", inputs.surge);
        let mut sway = raw("lateral_acceleration", inputs.sway).py_mul(&LegacyValue::Int(-1)).map_err(v::operation)?;
        let mut heave =
            raw("vertical_acceleration", inputs.heave).py_mul(&LegacyValue::Int(-1)).map_err(v::operation)?;
        let flag = |key, fallback: bool, equals| -> Result<bool, BeltError> {
            match profile {
                Some(profile) => Ok(openrigcue_model::python_compare::native_equal(
                    &LegacyValue::from_integer(v::item(profile, key)?.py_integer().map_err(v::operation)?),
                    &LegacyValue::Int(equals),
                )),
                None => Ok(if equals == 0 { !fallback } else { fallback }),
            }
        };
        if flag("belt_surge_enabled", tuning.surge_enabled, 0)? {
            surge = LegacyValue::Int(0);
        }
        if flag("belt_surge_reverse", tuning.surge_reverse, 1)? {
            surge = surge.py_mul(&LegacyValue::Int(-1)).map_err(v::operation)?;
        }
        if flag("belt_heave_enabled", tuning.heave_enabled, 0)? {
            heave = LegacyValue::Int(0);
        }
        if flag("belt_sway_curving_enabled", tuning.sway_enabled, 0)? || switches.motors == 1 {
            sway = LegacyValue::Int(0);
        }
        if flag("belt_sway_curving_reverse", tuning.sway_reverse, 1)? {
            sway = sway.py_mul(&LegacyValue::Int(-1)).map_err(v::operation)?;
        }
        if !switches.on {
            if switches.mode == BeltMode::Position {
                if self.homing(switches) == Homing::Calibrated {
                    bus.set_position(LEFT, SLACK_POSITION)?;
                    bus.set_position(RIGHT, -SLACK_POSITION)?;
                }
                initialization(bus, switches, BeltMode::Torque)?;
            }
            bus.set_torque(LEFT, 0)?;
            bus.set_torque(RIGHT, 0)?;
            return Ok(());
        }
        if self.homing(switches) == Homing::Requested {
            self.set_homing(switches, Homing::InProgress);
            if home_values(bus, switches, HOMING_TORQUE, calibration, settings)? {
                self.set_homing(switches, Homing::Calibrated);
            } else {
                switches.on = false;
                self.set_homing(switches, Homing::NotCalibrated);
                switches.test_mode = false;
            }
        } else if switches.test_mode && self.homing(switches) == Homing::Calibrated {
            let test_value = match &self.native_test_value {
                Some((value, projection)) if *projection == switches.test_value.to_bits() => {
                    value.py_as_double().map_err(v::operation)?
                }
                _ => switches.test_value,
            };
            let position = test_position(test_value)?;
            let offset = native_retraction_offset(settings, calibration.retraction)?;
            bus.set_position(LEFT, position - offset)?;
            bus.set_position(RIGHT, -(position - offset))?;
            // The test position is this cycle's only target.
            return Ok(());
        } else if switches.mode == BeltMode::Torque {
            initialization(bus, switches, BeltMode::Position)?;
            self.set_homing(switches, Homing::NotCalibrated);
        }
        if self.homing(switches) == Homing::Calibrated {
            self.stream_positions_values(bus, surge, sway, heave, tuning, maxima, calibration, profile, settings)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_positions_values(
        &mut self,
        bus: &mut DayondBus,
        surge: LegacyValue,
        sway: LegacyValue,
        heave: LegacyValue,
        tuning: &BeltTuning,
        maxima: &BeltMaxima,
        calibration: &BeltCalibration,
        profile: Option<&LegacyValue>,
        settings: Option<&LegacyValue>,
    ) -> Result<(), BeltError> {
        use super::values as v;
        let max = |key, fallback| {
            profile.map_or(Ok(LegacyValue::Float(fallback)), |p| v::field(p, "max_telemetry_vars", key))
        };
        let slider = |key, fallback| profile.map_or(Ok(LegacyValue::Float(fallback)), |p| v::item(p, key));
        let accelerating = surge.py_ge(&LegacyValue::Int(0)).map_err(v::operation)?;
        let max_surge = if accelerating {
            max("surge_acceleration", maxima.surge_acceleration)?
        } else {
            max("surge_deceleration", maxima.surge_deceleration)?
        };
        let raw_surge = native_effect_fraction(&surge, &max_surge, &slider("belt_surge", tuning.surge)?, None)?;
        let Some(surge_position) = self.surge.smooth_native(
            raw_surge,
            self.surge.previous,
            profile,
            "belt_surge_smoothing",
            tuning.surge_smoothing,
            "surge_position",
        ) else {
            return Ok(());
        };
        let raw_heave = native_effect_fraction(
            &heave,
            &max("heave", maxima.heave)?,
            &slider("belt_heave_factor", tuning.heave_factor)?,
            None,
        )?;
        let Some(heave_position) = self.heave.smooth_native(
            raw_heave,
            self.heave.previous,
            profile,
            "belt_heave_smoothing",
            tuning.heave_smoothing,
            "heave_position",
        ) else {
            return Ok(());
        };
        // The sway "previous value" entries are never updated, so they stay 0.
        let raw_left = native_effect_fraction(
            &sway,
            &max("sway", maxima.sway)?,
            &slider("belt_sway_curving_factor", tuning.sway_factor)?,
            Some(Side::Left),
        )?;
        let Some(left_position) = self.sway_left.smooth_native(
            raw_left,
            0.0,
            profile,
            "belt_sway_smoothing",
            tuning.sway_smoothing,
            "sway_left_position",
        ) else {
            return Ok(());
        };
        let final_left = target_steps(surge_position + heave_position + left_position)?;
        let raw_right = native_effect_fraction(
            &sway,
            &max("sway", maxima.sway)?,
            &slider("belt_sway_curving_factor", tuning.sway_factor)?,
            Some(Side::Right),
        )?;
        let Some(right_position) = self.sway_right.smooth_native(
            raw_right,
            0.0,
            profile,
            "belt_sway_smoothing",
            tuning.sway_smoothing,
            "sway_right_position",
        ) else {
            return Ok(());
        };
        let final_right = target_steps(surge_position + heave_position + right_position)?;
        let offset = native_retraction_offset(settings, calibration.retraction)?;
        bus.set_position(LEFT, final_left - offset)?;
        bus.set_position(RIGHT, -(final_right - offset))?;
        self.surge.previous = surge_position;
        self.heave.previous = heave_position;
        push_history(&mut self.surge.history, surge_position);
        push_history(&mut self.heave.history, heave_position);
        push_history(&mut self.sway_left.history, left_position);
        push_history(&mut self.sway_right.history, right_position);
        Ok(())
    }
}

fn test_position(test_value: f64) -> Result<i64, BeltError> {
    Ok(-truncate(map_clamped(test_value, 0.0, 100.0, 0.0, MAX_STEPS / 2.0))
        .ok_or(BeltError::NotFinite("belt test position"))?)
}

fn run_state(status: Option<(u16, DriveStatus)>) -> RunState {
    match status {
        Some((_, s)) if s.running => RunState::Start,
        Some(_) => RunState::Stop,
        None => RunState::NoResponse,
    }
}

/// One round of the bring-up retry loop: every drive still stopped is turned on and its status read
/// (0.5 s); a failed status request counts as no response, a failed turn-on ends the
/// round ("Looks like the motor is offline").
fn enable_round(bus: &mut DayondBus, left: &mut RunState, right: &mut RunState) -> Result<(), CmdError> {
    const STATUS_TIMEOUT: Duration = Duration::from_millis(500);
    for (drive, state) in [(LEFT, &mut *left), (RIGHT, &mut *right)] {
        if *state == RunState::Stop {
            bus.turn_on(drive)?;
            *state = bus.bus_status(drive, STATUS_TIMEOUT).map_or(RunState::NoResponse, run_state);
        }
    }
    debug!("status left {left:?} status right {right:?}");
    Ok(())
}

/// Enable both drives in `mode` and find out how many answer; false when none does.
///
/// Clear faults, select the mode, then up to 3 rounds of "turn on
/// and read the status (0.5 s)" for every drive still stopped; a single answering left drive makes this a
/// one-motor installation.
pub fn initialization(bus: &mut DayondBus, switches: &mut BeltSwitches, mode: BeltMode) -> Result<bool, BeltError> {
    let mut left = RunState::Stop;
    let mut right = RunState::Stop;
    bus.clear_fault(LEFT)?;
    bus.clear_fault(RIGHT)?;
    match mode {
        BeltMode::Torque => {
            bus.torque_mode(LEFT)?;
            bus.torque_mode(RIGHT)?;
        }
        BeltMode::Position => {
            bus.position_mode(LEFT, POSITION_MODE_RPM, POSITION_MODE_RAMP, POSITION_MODE_RAMP)?;
            bus.position_mode(RIGHT, POSITION_MODE_RPM, POSITION_MODE_RAMP, POSITION_MODE_RAMP)?;
        }
    }
    let mut rounds = 3;
    while left == RunState::Stop && right == RunState::Stop && rounds > 0 {
        if let Err(e) = enable_round(bus, &mut left, &mut right) {
            debug!("Looks like the motor is offline: {e}");
        }
        rounds -= 1;
    }
    if left == RunState::NoResponse && right == RunState::NoResponse {
        return Ok(false);
    }
    if left != RunState::Stop && right != RunState::Stop {
        if left == RunState::Start && right == RunState::NoResponse {
            switches.motors = 1;
        }
        switches.mode = mode;
        return Ok(true);
    }
    Ok(false)
}

/// Calibration outcome of one drive in a homing step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tension {
    Error,
    Ok,
    Waiting,
}

fn tension(current: f64, torque: i64) -> Tension {
    if current == -1.0 {
        Tension::Error
    } else if current >= (torque as f64 / 1000.0).abs() {
        Tension::Ok
    } else {
        Tension::Waiting
    }
}

/// Pull both belts tight in torque mode until the drives report the expected current at standstill.
///
/// The torque is `torque * factor`, truncated, with `factor` starting at
/// the power (0..100) mapped to 1.1..1.3 and dropping 0.05 per 50 ms step down to 1; done when each drive's
/// current reaches `|torque| / 1000` A and its speed is 0; gives up when the elapsed whole seconds exceed 5.
pub fn home(
    bus: &mut DayondBus,
    switches: &mut BeltSwitches,
    torque: i64,
    calibration: &BeltCalibration,
) -> Result<bool, BeltError> {
    home_values(bus, switches, torque, calibration, None)
}

fn home_values(
    bus: &mut DayondBus,
    switches: &mut BeltSwitches,
    torque: i64,
    calibration: &BeltCalibration,
    settings: Option<&LegacyValue>,
) -> Result<bool, BeltError> {
    debug!("Calibration Starting");
    if switches.mode == BeltMode::Position {
        initialization(bus, switches, BeltMode::Torque)?;
    }
    use super::values as v;
    let power = settings.map_or(Ok(LegacyValue::Int(calibration.power)), |settings| {
        v::field(settings, "BELT_TENSIONER", "belt_calibration_factor")
    })?;
    let power =
        LegacyValue::from_integer(power.py_integer().map_err(v::operation)?).py_as_double().map_err(v::operation)?;
    let mut factor = map_clamped(power, 0.0, 100.0, 1.1, 1.3);
    // The elapsed whole seconds are measured on the local wall clock.
    let start = bus.clock.local_now();
    let two = switches.motors == 2;
    let timed_out = loop {
        let torque_value = truncate(torque as f64 * factor).unwrap_or(0);
        factor = (factor - 0.05).max(1.0);
        bus.set_torque(LEFT, torque_value)?;
        if two {
            bus.set_torque(RIGHT, -torque_value)?;
        }
        bus.clock.sleep(Duration::from_millis(50));
        let left = tension(bus.output_current(LEFT), torque_value);
        let left_still = truncate(bus.output_speed(LEFT)) == Some(0);
        let done = if two {
            let right = tension(bus.output_current(RIGHT), torque_value);
            let right_still = truncate(bus.output_speed(RIGHT)) == Some(0);
            left == Tension::Ok && left_still && right == Tension::Ok && right_still
        } else {
            left == Tension::Ok && left_still
        };
        if done {
            break false;
        }
        if super::link::timedelta_seconds(bus.clock.local_now(), start) > 5 {
            break true;
        }
    };
    bus.set_torque(LEFT, 0)?;
    if two {
        bus.set_torque(RIGHT, 0)?;
    }
    if timed_out {
        return Ok(false);
    }
    initialization(bus, switches, BeltMode::Position)?;
    Ok(true)
}

/// USB ids of the bridge (CP210x, FTDI FT232).
const BRIDGE_IDS: [&str; 2] = ["10C4:EA60", "0403:6001"];
const BAUD_RATE: u32 = 115_200;
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const SETTLE_TIME: Duration = Duration::from_secs(2);

/// What `i_am` reported for a connected belt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeltIdentity {
    /// `belt_model` (the product byte).
    pub model: i64,
    /// `belt_version` (the model byte).
    pub version: i64,
    /// `belt_sn`.
    pub serial: String,
}

/// A connected belt.
pub struct ConnectedBelt {
    pub port: String,
    pub identity: BeltIdentity,
    pub bus: DayondBus,
}

/// Scan result.
pub struct BeltScan {
    pub connected: Option<ConnectedBelt>,
    /// Status to show when nothing connected (always set then; `None` only with a connected belt).
    pub status: Option<DeviceStatus>,
    /// Port to remember in the cache: it is stored before the drives are brought up, so a belt
    /// whose motors are offline is remembered too.
    pub remember_port: Option<String>,
}

/// Environment of a belt scan.
pub struct BeltScanContext<'a> {
    pub ports: &'a dyn PortLister,
    pub opener: &'a dyn PortOpener,
    pub clock: Arc<dyn Clock>,
    pub claimed: &'a ClaimedPorts,
    /// The port remembered from the last successful connection.
    pub cached_port: Option<&'a str>,
    /// `BELT_TENSIONER.com_port`.
    pub com_port: &'a str,
    pub allowed: &'a AllowedPorts,
    /// Publishes the belt status while the scan goes on (the firmware message
    /// and "Motor Offline" are set when such a device is met, before the next port is tried).
    pub report: &'a dyn Fn(DeviceStatus),
    /// The belt status the receive thread of a started bus watches (see [`BeltWatchdog`]).
    pub watchdog: Arc<dyn BeltWatchdog>,
}

/// Whether the next belt scan is the first one.
#[derive(Debug)]
pub struct BeltScanner {
    first_scan: bool,
}

impl Default for BeltScanner {
    fn default() -> Self {
        BeltScanner { first_scan: true }
    }
}

/// Probe result of one port.
enum Probe {
    Connected(ConnectedBelt),
    /// Genuine belt, drives silent; the port was remembered.
    MotorOffline(String),
    FirmwareTooOld,
    Rejected,
}

impl BeltScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look for the belt bridge and bring the drives up in torque mode.
    ///
    /// CP210x/FTDI ports at 115200 baud, `who_are_you` with a nonce and up to
    /// 10 reads for `i_am`, XTEA identity check (separator `'^'`), product 1, firmware 5 (older/other
    /// firmware: manual update), then the drive bring-up; afterwards the status is "Motor Offline" when
    /// only drives failed and "Not Connected" after any other pass that connected nothing. The status
    /// number of the answer is not judged.
    pub fn scan(
        &mut self,
        ctx: &BeltScanContext<'_>,
        switches: &mut BeltSwitches,
        rng: &mut impl rand::Rng,
    ) -> Result<BeltScan, PortScanError> {
        let rules = ScanRules {
            device: Device::Belt,
            exclusions: Exclusions::RaspberryUart,
            configured_port: ctx.com_port,
            allowed: ctx.allowed,
            claimed: ctx.claimed,
            cached_port: ctx.cached_port,
            first_scan: self.first_scan,
        };
        let mut last_status: Option<DeviceStatus> = None;
        let mut scan = BeltScan { connected: None, status: None, remember_port: None };
        for port in ctx.ports.ports()? {
            if !rules.may_try(&port) || !port.hwid_contains_any(&BRIDGE_IDS)? {
                continue;
            }
            match probe(ctx, &port, switches, rng) {
                Probe::Connected(belt) => {
                    scan.remember_port = Some(belt.port.clone());
                    scan.connected = Some(belt);
                    break;
                }
                Probe::MotorOffline(port) => {
                    scan.remember_port = Some(port);
                    (ctx.report)(DeviceStatus::MotorOffline);
                    last_status = Some(DeviceStatus::MotorOffline);
                }
                Probe::FirmwareTooOld => {
                    (ctx.report)(DeviceStatus::FirmwareUpdateRequired);
                    last_status = Some(DeviceStatus::FirmwareUpdateRequired);
                }
                Probe::Rejected => {}
            }
        }
        self.first_scan = false;
        if scan.connected.is_none() {
            // Every pass that connects nothing writes a status: "Motor Offline" stays, anything else
            // (a firmware message, or no belt met at all) becomes "Not Connected".
            scan.status = Some(if last_status == Some(DeviceStatus::MotorOffline) {
                DeviceStatus::MotorOffline
            } else {
                DeviceStatus::NotConnected
            });
        }
        Ok(scan)
    }
}

fn probe(ctx: &BeltScanContext<'_>, port: &PortEntry, switches: &mut BeltSwitches, rng: &mut impl rand::Rng) -> Probe {
    let settings = PortSettings { baud_rate: BAUD_RATE, read_timeout: Some(READ_TIMEOUT) };
    let mut link = match ctx.opener.open(&port.device, settings) {
        Ok(link) => link,
        Err(e) => {
            debug!("error setuping the board {e}");
            return Probe::Rejected;
        }
    };
    ctx.clock.sleep(SETTLE_TIME);
    let Some(answer) = ask_identity(link.as_mut(), COMMANDS, WHO_ARE_YOU, I_AM, rng) else { return Probe::Rejected };
    if answer.product != BELT_PRODUCT {
        debug!("This is not the Belt, disconnecting and trying next port");
        return Probe::Rejected;
    }
    if answer.version != SHIPPED_FIRMWARE {
        tracing::warn!("Belt firmware update required — manual flashing needed (Belt discontinued)");
        return Probe::FirmwareTooOld;
    }
    let identity = BeltIdentity { model: answer.product, version: answer.model, serial: answer.serial };
    let mut bus = match DayondBus::start(link, Arc::clone(&ctx.clock), Arc::clone(&ctx.watchdog)) {
        Ok(bus) => bus,
        Err(_) => return Probe::Rejected,
    };
    match initialization(&mut bus, switches, BeltMode::Torque) {
        Ok(true) => Probe::Connected(ConnectedBelt { port: port.device.clone(), identity, bus }),
        _ => {
            debug!("initialization failed");
            Probe::MotorOffline(port.device.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::link::fakes::{FakeLink, FakeOpener, FakePorts, ManualClock};
    use super::super::open_gate::{new_handshake, seal_for_tests};
    use super::*;
    use rand::SeedableRng;

    fn frames(written: &[u8]) -> Vec<Vec<u8>> {
        // Decode "2,<9 escaped bytes>;" frames into their 9 payload bytes.
        let mut out = Vec::new();
        let mut i = 0;
        while i < written.len() {
            assert_eq!(&written[i..i + 2], b"2,");
            i += 2;
            let mut payload = Vec::new();
            loop {
                let b = written[i];
                i += 1;
                match b {
                    b'/' => {
                        payload.push(written[i]);
                        i += 1;
                    }
                    b',' => {}
                    b';' => break,
                    other => payload.push(other),
                }
            }
            out.push(payload);
        }
        out
    }

    fn bus(link: &FakeLink) -> DayondBus {
        DayondBus::start(Box::new(link.clone()), Arc::new(ManualClock::default()), Arc::new(Status::default())).unwrap()
    }

    /// Write/read framing and the two-at-a-time queue.
    #[test]
    fn register_frames_and_queue() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        b.turn_on(LEFT).unwrap();
        b.set_position(RIGHT, -60_000).unwrap();
        b.set_torque(LEFT, -440).unwrap();
        b.clear_fault(RIGHT).unwrap();
        b.queue_write(LEFT, [reg::MODE, 0, MODE_TORQUE]).unwrap();
        assert_eq!(
            frames(&link.take_written()),
            vec![
                vec![1, 0, 0x1A, 0, 0, 1, 0, 0, 1],
                // -60000 = 0xFFFF15A0 big-endian
                vec![2, 1, 0x1A, 0x50, 0xFF, 0xFF, 0x05, 0x15, 0xA0],
                // -440 wraps to 0xFE48
                vec![1, 0, 0x1A, 0x08, 0xFE, 0x48, 0x08, 0xFE, 0x48],
                vec![2, 1, 0x1A, 0x4A, 0, 0, 0x4C, 0, 0],
            ]
        );
        // A single queued write leaves with the next one, under the next caller's drive.
        b.queue_write(RIGHT, [reg::ENABLE, 0, 1]).unwrap();
        assert_eq!(frames(&link.take_written()), vec![vec![2, 1, 0x1A, 2, 0, 0xC1, 0, 0, 1]]);
        b.position_mode(LEFT, 3000.0, 5, 5).unwrap();
        assert_eq!(
            frames(&link.take_written()),
            vec![vec![1, 0, 0x1A, 2, 0, 0xD0, 9, 5, 5], vec![1, 0, 0x1A, 0x51, 0, 0, 0x1D, 0x20, 0x00]]
        );
    }

    /// The receive thread's register cache and the getters' scaling.
    #[test]
    fn register_cache_and_getters() {
        let state = BusState::default();
        let msg = Message {
            command: Some(RECEIVED_STD_MESSAGE),
            args: [
                cmd_messenger::Value::Text("1".into()),
                cmd_messenger::Value::Int(8),
                cmd_messenger::Value::Int(0),
                cmd_messenger::Value::Int(0x2A),
                cmd_messenger::Value::Int(0xE2),
                cmd_messenger::Value::Int(0x01),
                cmd_messenger::Value::Int(0x2C),
                cmd_messenger::Value::Int(0xE4),
                cmd_messenger::Value::Int(0x00),
                cmd_messenger::Value::Int(0x40),
            ]
            .to_vec(),
        };
        assert_eq!(state.ingest(&msg), Some(()));
        assert_eq!(state.register(LEFT, 0xE2), Some(300));
        // The id is read as integer text: " +1 " is drive 1, 257 is not drive 1.
        let with_id = |id: &str| {
            let mut m = msg.clone();
            m.args[0] = cmd_messenger::Value::Text(id.into());
            m
        };
        let fresh = BusState::default();
        assert_eq!(fresh.ingest(&with_id("257")), Some(()));
        assert_eq!(fresh.register(LEFT, 0xE2), None);
        assert_eq!(fresh.ingest(&with_id("+1")), Some(()));
        assert_eq!(fresh.register(LEFT, 0xE2), Some(300));
        // A 2**64 id is stored where no drive looks; the frame still counts.
        assert_eq!(fresh.ingest(&with_id("18446744073709551616")), Some(()));
        // Malformed frames: a non-integer id, no arguments.
        assert_eq!(fresh.ingest(&with_id("x")), None);
        assert_eq!(fresh.ingest(&Message { command: Some(RECEIVED_STD_MESSAGE), args: Vec::new() }), None);
        let link = FakeLink::new();
        let b = bus(&link);
        b.state.ingest(&msg);
        assert_eq!(b.output_current(LEFT), 3.0);
        assert_eq!(b.output_speed(LEFT), 64.0 * 6000.0 / 16384.0);
        assert_eq!(b.output_current(RIGHT), -1.0);
        let s = DriveStatus::from_word(0b100_0001);
        assert!(s.running && s.overload && s.external_control && !s.overcurrent);
    }

    /// The belt status a test's receive thread watches.
    #[derive(Default)]
    struct Status {
        connected: AtomicBool,
        port: std::sync::Mutex<String>,
    }

    impl BeltWatchdog for Status {
        fn disconnect_if_connected(&self, _stop: &AtomicBool) -> bool {
            let was = self.connected.swap(false, Ordering::SeqCst);
            if was {
                self.port.lock().unwrap().clear();
            }
            was
        }
    }

    fn connected_status() -> Status {
        let status = Status::default();
        status.connected.store(true, Ordering::SeqCst);
        *status.port.lock().unwrap() = "/dev/ttyUSB7".into();
        status
    }

    /// A malformed received_std_message is skipped: the frame after it is still stored and the watchdog
    /// still ends the thread once the line falls silent.
    #[test]
    fn receive_thread_skips_a_malformed_frame() {
        struct Ticking(ManualClock);
        impl Clock for Ticking {
            fn now(&self) -> std::time::Instant {
                self.0.now()
            }
            fn sleep(&self, duration: Duration) {
                self.0.sleep(duration);
            }
            fn time(&self) -> f64 {
                self.0.advance(Duration::from_millis(500));
                self.0.time()
            }
        }
        let mut link = FakeLink::new();
        link.push_incoming(b"3;3,1,\x08,\x00,\x2a,\xe2,\x01,\x2d,\xe4,\x00,\x40;");
        let state = BusState::default();
        let status = connected_status();
        receive_loop(&mut link, &state, &Ticking(ManualClock::default()), &status);
        assert_eq!(state.register(LEFT, 0xE2), Some(301));
        assert!(!status.connected.load(Ordering::SeqCst));
        assert!(state.closed.load(Ordering::SeqCst) && state.stop.load(Ordering::SeqCst));
    }

    /// 3 s without a frame while "Connected": the receive thread itself sets "Not
    /// Connected" and an empty port, closes the board and stops. While not "Connected" it keeps reading.
    #[test]
    fn receive_thread_watchdog_disconnects_a_silent_belt() {
        struct Ticking(ManualClock);
        impl Clock for Ticking {
            fn now(&self) -> std::time::Instant {
                self.0.advance(Duration::from_millis(500));
                self.0.now()
            }
            fn sleep(&self, duration: Duration) {
                self.0.sleep(duration);
            }
        }
        let mut link = FakeLink::new();
        let state = BusState::default();
        let status = connected_status();
        receive_loop(&mut link, &state, &Ticking(ManualClock::default()), &status);
        assert!(!status.connected.load(Ordering::SeqCst));
        assert_eq!(*status.port.lock().unwrap(), "");
        assert!(state.closed.load(Ordering::SeqCst) && state.stop.load(Ordering::SeqCst));
    }

    /// Messages other than register frames (a log line, an unknown command) also restart the 3 s silence:
    /// with the clock stepping 500 ms per reading, a stream of them is read to its end before the watchdog
    /// disconnects the belt.
    #[test]
    fn receive_thread_watchdog_counts_every_message_as_traffic() {
        struct Ticking(ManualClock);
        impl Clock for Ticking {
            fn now(&self) -> std::time::Instant {
                self.0.now()
            }
            fn sleep(&self, duration: Duration) {
                self.0.sleep(duration);
            }
            fn time(&self) -> f64 {
                self.0.advance(Duration::from_millis(500));
                self.0.time()
            }
        }
        let mut link = FakeLink::new();
        for _ in 0..10 {
            link.push_incoming(b"5,drive log;x,1;");
        }
        let state = BusState::default();
        let status = connected_status();
        receive_loop(&mut link, &state, &Ticking(ManualClock::default()), &status);
        assert!(link.0.lock().unwrap().incoming.is_empty(), "the watchdog fired while messages were arriving");
        assert!(!status.connected.load(Ordering::SeqCst));
        assert!(state.closed.load(Ordering::SeqCst) && state.stop.load(Ordering::SeqCst));
    }

    /// The unplugged-port errors close the board and end the receive thread;
    /// the belt is not declared lost (the connection scan's port-loss check changes the status later).
    #[test]
    fn receive_thread_ends_when_the_port_is_gone() {
        let mut link = FakeLink::new();
        link.0.lock().unwrap().read_error = Some((std::io::ErrorKind::BrokenPipe, "Broken pipe"));
        let state = BusState::default();
        let status = connected_status();
        receive_loop(&mut link, &state, &ManualClock::default(), &status);
        assert!(status.connected.load(Ordering::SeqCst));
        assert!(state.closed.load(Ordering::SeqCst));
        let mut b = bus(&link);
        b.state.closed.store(true, Ordering::SeqCst);
        assert!(b.set_position(LEFT, 10).is_err(), "a closed board takes no frames");
    }

    /// Effect fraction, sway fraction and moving average.
    #[test]
    fn effect_math() {
        // Only negative input (braking, negated heave) tightens the belt.
        assert_eq!(effect_fraction(-2.5, 5.0, 10.0), 0.25);
        assert_eq!(effect_fraction(2.5, 5.0, 10.0), 0.0);
        assert_eq!(effect_fraction(0.0, 5.0, 10.0), 0.0);
        assert_eq!(effect_fraction(-2.5, 0.0, 10.0), 0.0);
        assert!(effect_fraction(f64::NAN, 5.0, 10.0).is_nan());
        assert_eq!(sway_fraction(Side::Left, -5.0, 5.0, 20.0), 1.0);
        assert_eq!(sway_fraction(Side::Left, 5.0, 5.0, 20.0), 0.0);
        assert_eq!(sway_fraction(Side::Right, 5.0, 5.0, 10.0), 0.5);
        let hist: VecDeque<f64> = [1.0, 2.0, 3.0, 4.0].into_iter().collect();
        // smoothing 15 -> window 3; history longer than 3 -> (4 + 3 + 2 + 10) / 4
        assert_eq!(moving_average(10.0, &hist, 15).unwrap(), 19.0 / 4.0);
        // history exactly as long as the window: unchanged
        let three: VecDeque<f64> = [1.0, 2.0, 3.0].into_iter().collect();
        assert_eq!(moving_average(10.0, &three, 15).unwrap(), 10.0);
        assert_eq!(moving_average(10.0, &hist, 0).unwrap(), 10.0);
        assert_eq!(moving_average(10.0, &VecDeque::new(), 15).unwrap(), 10.0);
        assert_eq!(moving_average(7.0, &hist, -10).unwrap(), -7.0);
        assert_eq!(moving_average(7.0, &hist, -15).unwrap(), -3.5);
        assert_eq!(moving_average(7.0, &VecDeque::new(), -5).unwrap(), 7.0);
        assert!(matches!(moving_average(7.0, &hist, -5), Err(BeltError::DivisionByZero)));
        assert!(moving_average(-0.0, &hist, 0).unwrap().is_sign_positive());
    }

    fn tuning() -> BeltTuning {
        BeltTuning {
            surge_enabled: true,
            surge_reverse: false,
            surge: 10.0,
            surge_smoothing: 0,
            heave_enabled: true,
            heave_factor: 10.0,
            heave_smoothing: 0,
            sway_enabled: true,
            sway_reverse: false,
            sway_factor: 10.0,
            sway_smoothing: 0,
        }
    }

    const MAXIMA: BeltMaxima = BeltMaxima { surge_acceleration: 5.0, surge_deceleration: 5.0, heave: 2.0, sway: 5.0 };
    const CALIBRATION: BeltCalibration = BeltCalibration { power: 50, retraction: 50 };

    /// Streaming positions and the switch-off path.
    #[test]
    fn stream_positions_and_switch_off() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        let mut state = BeltState::new();
        let mut switches =
            BeltSwitches { on: true, homing: Homing::Calibrated, mode: BeltMode::Position, ..Default::default() };
        // Accelerating (surge > 0) does not pull.
        state
            .send_stream(
                &mut b,
                &mut switches,
                BeltInputs { surge: 2.6, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION,
            )
            .unwrap();
        let f = frames(&link.take_written());
        // 0 steps: left 0 + 50000 = 0x0000C350, right -(50000)
        assert_eq!(f[0], vec![1, 0, 0x1A, 0x50, 0, 0, 0x05, 0xC3, 0x50]);
        // braking: surge -2.6 of max 5 -> 0.26 -> 20000 steps; offset -(20000 + 30000) = -50000
        state
            .send_stream(
                &mut b,
                &mut switches,
                BeltInputs { surge: -2.6, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION,
            )
            .unwrap();
        let f = frames(&link.take_written());
        // left: -20000 + 50000 = 30000 = 0x00007530; right: -(-20000 + 50000)
        assert_eq!(f[0], vec![1, 0, 0x1A, 0x50, 0, 0, 0x05, 0x75, 0x30]);
        assert_eq!(f[1], vec![2, 1, 0x1A, 0x50, 0xFF, 0xFF, 0x05, 0x8A, 0xD0]);
        switches.on = false;
        state
            .send_stream(
                &mut b,
                &mut switches,
                BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION,
            )
            .unwrap();
        let f = frames(&link.take_written());
        // slack positions, then torque-mode initialization (clear faults, mode frames, turn-on, reads), zero torque
        assert_eq!(f[0], vec![1, 0, 0x1A, 0x50, 0x00, 0x03, 0x05, 0x0D, 0x40]);
        assert_eq!(f.last().unwrap(), &vec![2, 1, 0x1A, 0x08, 0, 0, 0x08, 0, 0]);
    }

    #[test]
    fn negative_smoothing_zero_division_is_caught_and_drops_the_frame() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        let mut state = BeltState::new();
        let mut switches =
            BeltSwitches { on: true, homing: Homing::Calibrated, mode: BeltMode::Position, ..Default::default() };
        let mut tuning = tuning();
        state
            .send_stream(
                &mut b,
                &mut switches,
                BeltInputs { surge: -2.5, sway: 0.0, heave: 0.0 },
                &tuning,
                &MAXIMA,
                &CALIBRATION,
            )
            .unwrap();
        assert!(!link.take_written().is_empty());
        let previous = state.surge.previous;
        let history = state.surge.history.clone();
        tuning.surge_smoothing = -5;
        let result = state.send_stream(
            &mut b,
            &mut switches,
            BeltInputs { surge: -3.0, sway: 0.0, heave: 0.0 },
            &tuning,
            &MAXIMA,
            &CALIBRATION,
        );
        // -5 / 5 truncated, plus 1, is 0: the division by zero is logged, and the frame ends there: nothing
        // is sent, and the previous value and the history stay as they were.
        result.unwrap();
        assert!(link.take_written().is_empty());
        assert_eq!(state.surge.previous, previous);
        assert_eq!(state.surge.history, history);
    }

    #[test]
    fn native_belt_test_scalar_map_clamps_before_integer_conversion() {
        for (input, expected) in [
            (-10.0, 0),
            (17.0, -8500),
            (110.0, -50000),
            (f64::NEG_INFINITY, 0),
            (f64::INFINITY, -50000),
            (f64::NAN, -50000),
        ] {
            assert_eq!(test_position(input).unwrap(), expected, "input {input:?}");
        }
    }

    #[test]
    fn native_belt_test_scalar_stream_and_retraction_maps_clamp() {
        assert_eq!(target_steps(2.0).unwrap(), -100000);
        assert_eq!(target_steps(-1.0).unwrap(), 0);
        assert_eq!(target_steps(f64::NAN).unwrap(), -100000);
        assert_eq!(retraction_offset(-10), -20000);
        assert_eq!(retraction_offset(110), -80000);
    }

    /// The rescale clamps NaN/infinity before the integer conversion; both sends and history updates continue.
    #[test]
    fn non_finite_effects_are_clamped_and_sent() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        let mut state = BeltState::new();
        let mut switches =
            BeltSwitches { on: true, homing: Homing::Calibrated, mode: BeltMode::Position, ..Default::default() };
        for surge in [f64::NAN, f64::NEG_INFINITY] {
            let result = state.send_stream(
                &mut b,
                &mut switches,
                BeltInputs { surge, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION,
            );
            result.unwrap();
            assert!(!link.take_written().is_empty());
        }
        assert_eq!(state.surge.history.len(), 2);
        switches.test_mode = true;
        switches.test_value = f64::NAN;
        let result = state.send_stream(
            &mut b,
            &mut switches,
            BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
            &tuning(),
            &MAXIMA,
            &CALIBRATION,
        );
        result.unwrap();
        assert!(!link.take_written().is_empty());
    }

    #[test]
    fn native_belt_test_scalar_is_converted_only_in_the_test_branch() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        let mut state = BeltState::new();
        let mut switches =
            BeltSwitches { homing: Homing::Calibrated, test_mode: true, test_value: 17.0, ..Default::default() };
        for value in [LegacyValue::Str("17".into()), LegacyValue::Bytes(b"17".to_vec())] {
            state.assign_native_test_value(value.clone(), 17.0);
            switches.on = false;
            state
                .send_stream(
                    &mut b,
                    &mut switches,
                    BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                    &tuning(),
                    &MAXIMA,
                    &CALIBRATION,
                )
                .unwrap();
            assert!(!link.take_written().is_empty());
            switches.on = true;
            let result = state.send_stream(
                &mut b,
                &mut switches,
                BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION,
            );
            let error = result.unwrap_err();
            assert!(matches!(&error, BeltError::Native(native) if native.class == "TypeError"), "{error:?}");
            assert_eq!(error.to_string(), format!("must be real number, not {}", value.type_name()), "str(e)");
            assert!(link.take_written().is_empty());
            assert!(state.surge.history.is_empty());
        }
        state.assign_native_test_value(LegacyValue::Bool(true), 1.0);
        switches.test_value = 1.0;
        state
            .send_stream(
                &mut b,
                &mut switches,
                BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION,
            )
            .unwrap();
        assert!(!link.take_written().is_empty());
    }

    /// `belt_homing = 3` precedes the first I/O in `home`, including its error path.
    #[test]
    fn live_belt_homing_reads_the_published_value_instead_of_a_stale_switch_mirror() {
        for (published, mirror, sends) in
            [(Homing::NotCalibrated, Homing::Calibrated, false), (Homing::Calibrated, Homing::NotCalibrated, true)]
        {
            let link = FakeLink::new();
            let mut bus = bus(&link);
            let status = PublishedHoming::default();
            status.set(published);
            let mut state = BeltState::with_homing_status(status);
            let mut switches =
                BeltSwitches { on: true, homing: mirror, mode: BeltMode::Position, ..Default::default() };
            state
                .send_stream(
                    &mut bus,
                    &mut switches,
                    BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                    &tuning(),
                    &MAXIMA,
                    &CALIBRATION,
                )
                .unwrap();
            assert_eq!(!link.take_written().is_empty(), sends);
        }
    }

    #[test]
    fn live_belt_homing_rereads_after_test_positions_before_normal_streaming() {
        struct ResetOnWrite {
            link: FakeLink,
            homing: PublishedHoming,
        }
        impl SerialLink for ResetOnWrite {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let written = self.link.write(bytes)?;
                self.homing.set(Homing::NotCalibrated);
                Ok(written)
            }
            fn read_byte(&mut self) -> std::io::Result<Option<u8>> {
                self.link.read_byte()
            }
            fn read_available(&mut self) -> std::io::Result<Vec<u8>> {
                self.link.read_available()
            }
            fn bytes_waiting(&mut self) -> std::io::Result<usize> {
                self.link.bytes_waiting()
            }
            fn try_clone(&self) -> std::io::Result<Box<dyn SerialLink>> {
                self.link.try_clone()
            }
        }
        let link = FakeLink::new();
        let status = PublishedHoming::default();
        status.set(Homing::Calibrated);
        let mut bus = bus(&link);
        bus.link = Box::new(ResetOnWrite { link: link.clone(), homing: status.clone() });
        let mut state = BeltState::with_homing_status(status);
        let mut switches = BeltSwitches {
            on: true,
            homing: Homing::Calibrated,
            mode: BeltMode::Position,
            test_mode: true,
            test_value: 17.0,
            ..Default::default()
        };
        state
            .send_stream(
                &mut bus,
                &mut switches,
                BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION,
            )
            .unwrap();
        assert_eq!(
            frames(&link.take_written()).len(),
            2,
            "the reset skips normal positions after the two test targets"
        );
        assert!(state.surge.history.is_empty());
    }

    #[test]
    fn live_belt_homing_published_request_starts_before_the_first_failed_write() {
        let link = FakeLink::new();
        let mut bus = bus(&link);
        link.0.lock().unwrap().fail_writes = true;
        let status = PublishedHoming::default();
        status.set(Homing::Requested);
        let mut state = BeltState::with_homing_status(status.clone());
        let mut switches =
            BeltSwitches { on: true, homing: Homing::NotCalibrated, mode: BeltMode::Position, ..Default::default() };
        assert!(state
            .send_stream(
                &mut bus,
                &mut switches,
                BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                &tuning(),
                &MAXIMA,
                &CALIBRATION
            )
            .is_err());
        assert_eq!(status.get(), Homing::InProgress);
    }

    #[test]
    fn homing_progress_is_published_before_a_failed_device_write() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        link.0.lock().unwrap().fail_writes = true;
        let status = PublishedHoming::default();
        status.set(Homing::Requested);
        let mut state = BeltState::with_homing_status(status.clone());
        let mut switches = BeltSwitches { on: true, homing: Homing::Requested, ..Default::default() };
        let result = state.send_stream(
            &mut b,
            &mut switches,
            BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
            &tuning(),
            &MAXIMA,
            &CALIBRATION,
        );
        assert!(result.is_err());
        assert_eq!(switches.homing, Homing::InProgress);
        assert_eq!(status.get(), Homing::InProgress);
    }

    #[test]
    fn stream_publishes_calibration_progress_and_both_outcomes() {
        struct ObservingClock {
            clock: ManualClock,
            status: PublishedHoming,
            observed: AtomicBool,
        }
        impl Clock for ObservingClock {
            fn now(&self) -> std::time::Instant {
                self.clock.now()
            }
            fn sleep(&self, duration: Duration) {
                if duration == Duration::from_millis(50) {
                    assert_eq!(self.status.get(), Homing::InProgress);
                    self.observed.store(true, Ordering::SeqCst);
                }
                self.clock.sleep(duration);
            }
        }
        for ready in [false, true] {
            let link = FakeLink::new();
            let mut b = bus(&link);
            if ready {
                for drive in [LEFT, RIGHT] {
                    let regs = HashMap::from([
                        (reg::OUTPUT_CURRENT, 60u16),
                        (reg::OUTPUT_SPEED, 0),
                        (reg::BUS_VOLTAGE, 240),
                        (reg::STATUS, 1),
                    ]);
                    b.state.registers.lock().unwrap().insert(i64::from(drive.arbitration_id), regs);
                }
            }
            let status = PublishedHoming::default();
            status.set(Homing::Requested);
            let clock = Arc::new(ObservingClock {
                clock: ManualClock::default(),
                status: status.clone(),
                observed: AtomicBool::new(false),
            });
            b.clock = clock.clone();
            let mut state = BeltState::with_homing_status(status.clone());
            let mut switches = BeltSwitches { on: true, homing: Homing::Requested, ..Default::default() };
            state
                .send_stream(
                    &mut b,
                    &mut switches,
                    BeltInputs { surge: 0.0, sway: 0.0, heave: 0.0 },
                    &tuning(),
                    &MAXIMA,
                    &CALIBRATION,
                )
                .unwrap();
            assert!(clock.observed.load(Ordering::SeqCst));
            assert_eq!(status.get(), if ready { Homing::Calibrated } else { Homing::NotCalibrated });
            assert_eq!(status.get(), switches.homing);
        }
    }

    /// Bring-up: no drive answers -> false; left answers alone -> one-motor installation.
    #[test]
    fn initialization_counts_motors() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        let mut switches = BeltSwitches::default();
        assert!(!initialization(&mut b, &mut switches, BeltMode::Torque).unwrap());
        let mut regs = HashMap::new();
        regs.insert(reg::BUS_VOLTAGE, 240u16);
        regs.insert(reg::STATUS, 1u16);
        b.state.registers.lock().unwrap().insert(i64::from(LEFT.arbitration_id), regs);
        assert!(initialization(&mut b, &mut switches, BeltMode::Position).unwrap());
        assert_eq!(switches.motors, 1);
        assert_eq!(switches.mode, BeltMode::Position);
    }

    /// Bring-up: write failures inside the retry loop are that round's failure
    /// ("Looks like the motor is offline", the round still counts) or a "no_response" status (a failed
    /// status read); only failures of the clear-fault and mode writes before the loop are errors.
    #[test]
    fn initialization_survives_offline_drives() {
        // Writes 1-4: clear_fault left/right and the torque-mode frames; write 5 is the left turn-on.
        let link = FakeLink::new();
        let mut b = bus(&link);
        let mut switches = BeltSwitches::default();
        link.0.lock().unwrap().writes_before_failure = Some(4);
        assert!(!initialization(&mut b, &mut switches, BeltMode::Torque).unwrap());
        assert_eq!(frames(&link.written()).len(), 4);
        // Left turn-on succeeds, its status request fails (no_response), the right turn-on fails: the loop
        // ends after that round (left no longer 'stop') with left no_response / right stop: no drive answered.
        let link = FakeLink::new();
        let mut b = bus(&link);
        link.0.lock().unwrap().writes_before_failure = Some(5);
        assert!(!initialization(&mut b, &mut switches, BeltMode::Torque).unwrap());
        assert_eq!(frames(&link.written()).len(), 5);
        // A failing clear-fault write (before the retry loop) is an error.
        let link = FakeLink::new();
        let mut b = bus(&link);
        link.0.lock().unwrap().fail_writes = true;
        assert!(initialization(&mut b, &mut switches, BeltMode::Torque).is_err());
    }

    /// Homing: both drives tight and still -> success; silent drives -> timeout after > 5 s.
    #[test]
    fn homing_success_and_timeout() {
        let link = FakeLink::new();
        let mut b = bus(&link);
        let mut switches = BeltSwitches { on: true, motors: 2, mode: BeltMode::Torque, ..Default::default() };
        assert!(!home(&mut b, &mut switches, HOMING_TORQUE, &CALIBRATION).unwrap());
        for drive in [LEFT, RIGHT] {
            let mut regs = HashMap::new();
            regs.insert(reg::OUTPUT_CURRENT, 60u16);
            regs.insert(reg::OUTPUT_SPEED, 0u16);
            regs.insert(reg::BUS_VOLTAGE, 240u16);
            regs.insert(reg::STATUS, 1u16);
            b.state.registers.lock().unwrap().insert(i64::from(drive.arbitration_id), regs);
        }
        link.take_written();
        assert!(home(&mut b, &mut switches, HOMING_TORQUE, &CALIBRATION).unwrap());
        // First step: torque -400 * 1.2 = -480 = 0xFE20 on the left, +480 on the right.
        let f = frames(&link.take_written());
        assert_eq!(f[0], vec![1, 0, 0x1A, 0x08, 0xFE, 0x20, 0x08, 0xFE, 0x20]);
        assert_eq!(f[1], vec![2, 1, 0x1A, 0x08, 0x01, 0xE0, 0x08, 0x01, 0xE0]);
        assert_eq!(switches.mode, BeltMode::Position);
    }

    /// Scan: a receive error goes back to the top of the loop without
    /// counting, so twelve malformed answers still leave room for the genuine `i_am`.
    #[test]
    fn identity_receive_errors_are_not_attempts() {
        let mut link = FakeLink::new();
        let mut probe_rng = rand::rngs::SmallRng::seed_from_u64(9);
        let nonce = new_handshake(&mut probe_rng);
        let mut reply = b"1,x;".repeat(12);
        reply.extend(format!("1,{},\x01,\x02,\x05,\x01,SN;", seal_for_tests(&format!("{nonce}^1"))).as_bytes());
        link.queue_reply(&reply);
        let mut rng = rand::rngs::SmallRng::seed_from_u64(9);
        let answer = ask_identity(&mut link, COMMANDS, WHO_ARE_YOU, I_AM, &mut rng).expect("identity");
        assert_eq!((answer.product, answer.version, answer.serial.as_str()), (1, 5, "SN"));
    }

    /// Drive frames the bridge forwards before its `i_am` are waited past without counting, so twelve of
    /// them still leave room for the genuine answer; twelve other messages use up the ten attempts.
    #[test]
    fn identity_forwarded_drive_frames_are_not_attempts() {
        let answer_after = |noise: &[u8]| {
            let mut link = FakeLink::new();
            let mut probe_rng = rand::rngs::SmallRng::seed_from_u64(9);
            let nonce = new_handshake(&mut probe_rng);
            let mut reply = noise.repeat(12);
            reply.extend(format!("1,{},\x01,\x02,\x05,\x01,SN;", seal_for_tests(&format!("{nonce}^1"))).as_bytes());
            link.queue_reply(&reply);
            let mut rng = rand::rngs::SmallRng::seed_from_u64(9);
            ask_identity(&mut link, COMMANDS, WHO_ARE_YOU, I_AM, &mut rng)
        };
        let answer = answer_after(b"3,1,\x08,\x00,\x2a,\xe2,\x01,\x2d,\xe4,\x00,\x40;").expect("identity");
        assert_eq!((answer.product, answer.version, answer.serial.as_str()), (1, 5, "SN"));
        assert!(answer_after(b"5;").is_none());
    }

    /// Scan: genuine bridge (nonce) and firmware checks; a status number other than 1 does not
    /// reject the belt.
    #[test]
    fn scan_connects_and_rejects() {
        let link = FakeLink::new();
        let opener = FakeOpener::with("/dev/ttyUSB7", &link);
        let ports = FakePorts(vec![PortEntry::usb("/dev/ttyUSB7", 0x10c4, 0xea60, None, None)]);
        let claimed = ClaimedPorts::default();
        let allowed = AllowedPorts::default();
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        let reported = std::cell::RefCell::new(Vec::new());
        let ctx = BeltScanContext {
            ports: &ports,
            opener: &opener,
            clock,
            claimed: &claimed,
            cached_port: None,
            com_port: "",
            allowed: &allowed,
            report: &|status| reported.borrow_mut().push(status),
            watchdog: Arc::new(Status::default()),
        };
        // The nonce of the seeded generator is known only at run time, so answer with a wrong one first.
        link.queue_reply(format!("1,{},\x01,\x02,\x05,\x01,SN;", seal_for_tests("xxxxxxxxxxxxxx^1")).as_bytes());
        let mut rng = rand::rngs::SmallRng::seed_from_u64(3);
        let mut switches = BeltSwitches::default();
        let scan = BeltScanner::new().scan(&ctx, &mut switches, &mut rng).unwrap();
        assert!(scan.connected.is_none() && scan.remember_port.is_none());
        assert_eq!(scan.status, Some(DeviceStatus::NotConnected));
        // Correct nonce: run the same seeded generator to build the answer.
        let mut probe_rng = rand::rngs::SmallRng::seed_from_u64(3);
        let nonce = new_handshake(&mut probe_rng);
        link.queue_reply(format!("1,{},\x01,\x02,\x04,\x01,SN;", seal_for_tests(&format!("{nonce}^0"))).as_bytes());
        let mut rng = rand::rngs::SmallRng::seed_from_u64(3);
        let scan = BeltScanner::new().scan(&ctx, &mut switches, &mut rng).unwrap();
        assert!(scan.connected.is_none());
        assert_eq!(scan.status, Some(DeviceStatus::NotConnected));
        // belt_status shows the firmware message as soon as that belt is met, before the scan ends.
        assert_eq!(*reported.borrow(), vec![DeviceStatus::FirmwareUpdateRequired]);
    }
}

//! SMC3 controllers as used by DOF Reality platforms.
//!
//! Port scan and identification handshake (connect), the per-frame
//! position stream with automatic Kp updates (stream and test frames), disconnect, and the UDP mirror
//! used in debug mode. A 6-motor platform (H6, P6) has two boxes; the box's own `[v6d]` answer, not the
//! port order, decides which one is box 1 (left, frame `[A][B][C][Y]`) and box 2 (right, frame `[A][B][C]`).
//!
//! The position-feedback helpers in [`diagnostics`] and [`identify_port`] are for bring-up
//! tools; the driver itself never sends `[rd<motor>]`. They are opt-in and not used by [`MotionDriver`].

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tracing::debug;

use super::clock::Clock;
use super::driver_support::boolean_setting_error;
use super::native::{self, NativeSetting};
use super::{
    python_print, ActuatorPositions, ControllerIdentity, DatagramSocket, DriverFactory, LinkStatus, LiveSettings,
    MotionContext, MotionDriver, MotionError, MotionHost, PyFrame,
};
use crate::discovery::{scan_may_probe, PortInfo};
use crate::serial::{Link, LinkError, OpenOptions};
use num_bigint::BigInt;
use openrigcue_model::native_value::LegacyValue;

pub const BAUD_RATE: u32 = 500_000;

/// The file name in the frames an error of this driver passes.
const SMC3_FILE: &str = "backend/controllers/smc3.py";

/// The frame for `function` and `line` in this driver's file.
const fn at(function: &'static str, line: u32) -> PyFrame {
    PyFrame::new(SMC3_FILE, function, line)
}

/// The error of a setting that cannot be read as a boolean at `frame`.
fn switch_error(frame: PyFrame) -> impl FnOnce(MotionError) -> MotionError {
    move |error| match error {
        MotionError::Native(detail) => boolean_setting_error(&detail, frame),
        other => other.in_frame(frame),
    }
}

/// Open `path` exclusively or shared, as [`Smc3Settings::exclusive_open`] says.
fn open_port(
    host: &mut dyn MotionHost,
    path: &str,
    options: &OpenOptions,
    exclusive: bool,
) -> Result<Box<dyn Link>, LinkError> {
    if exclusive {
        host.open_serial(path, options)
    } else {
        host.open_serial_shared(path, options)
    }
}

/// Sending sockets of the box-1 and box-2 debug mirror.
const MIRROR_SOCKETS: [DatagramSocket; 2] = [DatagramSocket("smc3-box1-mirror"), DatagramSocket("smc3-box2-mirror")];
/// The write timeout of the scanned ports.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
pub const DEFAULT_LEFT_DEBUG_PORT: u16 = 20017;
pub const DEFAULT_RIGHT_DEBUG_PORT: u16 = 20018;

/// Pause after every query and configuration write.
const COMMAND_PAUSE: Duration = Duration::from_millis(100);
/// Linux pre-connection reset: pause after the open–flush–close.
const PRE_RESET_PAUSE: Duration = Duration::from_millis(1500);
/// How long the `[ver]` probe of one port lasts at most.
const PROBE_WINDOW: Duration = Duration::from_secs(10);
/// First `[ver]` is sent this long after opening (the board resets on open).
const BOOT_DELAY: Duration = Duration::from_secs(2);
const RESEND_INTERVAL: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Give up on a port that sent more than this many bytes without any `[`.
const MAX_UNFRAMED_BYTES: usize = 128;
/// Lowest `[ver]` version accepted as an SMC3.
const MIN_VERSION: i64 = 100;
const DISCONNECT_PAUSE: Duration = Duration::from_secs(1);
/// Kp written = `[MOTIONBOX] reaction_speed` + 20 (SMC3 ×100 convention).
const KP_OFFSET: i64 = 20;
/// Ki written (0.01 in the ×100 convention).
const KI_VALUE: i64 = 1;
/// Models with this many motors use two boxes.
const TWO_BOX_MOTORS: usize = 6;

/// Frame scaling of a box: its controller resolution. Positions are scaled to 10 bit
/// only when it is exactly 10; any other value sends them unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Resolution(pub i64);

impl Resolution {
    pub const TEN_BIT: Resolution = Resolution(10);
    pub const TWELVE_BIT: Resolution = Resolution(12);

    fn scales_to_ten_bit(self) -> bool {
        self == Resolution::TEN_BIT
    }
}

/// Box 1 (left; motors 1–3 and the 4th channel `Y`) or box 2 (right; motors 4–6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BoxSlot {
    Left,
    Right,
}

impl BoxSlot {
    const BOTH: [BoxSlot; 2] = [BoxSlot::Left, BoxSlot::Right];

    /// Index in the motion ports and in the controller list.
    pub fn index(self) -> usize {
        match self {
            BoxSlot::Left => 0,
            BoxSlot::Right => 1,
        }
    }
}

/// Decoded `[ver]` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct VersionReply {
    /// Leading digit of a 4-digit value, else -1. It is published as the reported number of motors when the
    /// box also answers `[vmd]`.
    pub leading_digit: i64,
    /// Remaining three digits of a 4-digit value, the value itself when it has 3 digits, else -1.
    pub version: i64,
}

/// Pure encoding and decoding of SMC3 frames.
pub mod wire {
    use super::{at, BoxSlot, Resolution, VersionReply};
    use crate::motion::{native, MotionError};
    use openrigcue_model::native_value::LegacyValue;
    use tracing::debug;

    /// The frame of the two-byte big-endian packing in a single-value command.
    pub(super) const GEN_SMC3_COMMAND_TO_BYTES: crate::motion::PyFrame = at("gen_smc3_command", 83);
    /// The first packing line of the four-letter position command (`A`, `B`, `C`, `Y`) and of
    /// the three-letter one (`A`, `B`, `C`).
    const GEN_ABCY_FIRST_LINE: u32 = 113;
    const GEN_ABC_FIRST_LINE: u32 = 94;

    /// The overflow error of packing a value outside 0..=65535 into two unsigned bytes.
    pub(super) fn overflow(negative: bool) -> MotionError {
        MotionError::Native(
            if negative {
                "OverflowError: can't convert negative int to unsigned"
            } else {
                "OverflowError: int too big to convert"
            }
            .into(),
        )
    }

    /// Reply positions whose low byte the update test compares (1st, 2nd and 3rd frame).
    const GAIN_POSITIONS: [usize; 3] = [3, 8, 13];
    /// Low byte of the 4th reply frame (`p` / `i` channel), which the connect compares on its own.
    const FOURTH_GAIN_POSITION: usize = 18;

    /// `[<letter> hi lo]`; the value must fit 16 bits.
    pub fn frame(letter: u8, value: i64) -> Result<[u8; 5], MotionError> {
        let value16 = u16::try_from(value).map_err(|_| overflow(value < 0))?;
        let [hi, lo] = value16.to_be_bytes();
        Ok([b'[', letter, hi, lo, b']'])
    }

    /// The same value for every letter; a value
    /// that does not fit is an error in the loop.
    pub fn repeated(letters: &[u8], value: i64) -> Result<Vec<u8>, MotionError> {
        let mut out = Vec::with_capacity(letters.len() * 5);
        for &letter in letters {
            out.extend_from_slice(&frame(letter, value).map_err(|e| e.in_frame(GEN_SMC3_COMMAND_TO_BYTES))?);
        }
        Ok(out)
    }

    /// Position frames for `letters[i]` with `values[i]`, scaled to 10 bit for a 10-bit box:
    /// one packing line per
    /// letter, which is the frame of a value that does not fit.
    pub fn positions(letters: &[u8], values: &[i64], resolution: Resolution) -> Result<Vec<u8>, MotionError> {
        let (function, first_line) = if letters.contains(&b'Y') {
            ("gen_abcy_command", GEN_ABCY_FIRST_LINE)
        } else {
            ("gen_abc_command", GEN_ABC_FIRST_LINE)
        };
        let mut out = Vec::with_capacity(letters.len() * 5);
        for ((&letter, &value), line) in letters.iter().zip(values).zip(first_line..) {
            let sent = if resolution.scales_to_ten_bit() { to_ten_bit(value) } else { value };
            out.extend_from_slice(&frame(letter, sent).map_err(|e| e.in_frame(at(function, line)))?);
        }
        Ok(out)
    }

    /// The first line of the rescale from 0..4095 to 0..1023 in the four-letter position command
    /// and in the three-letter one, for a 10-bit box.
    const GEN_ABCY_FIRST_SCALE_LINE: u32 = 106;
    const GEN_ABC_FIRST_SCALE_LINE: u32 = 90;

    /// `[<letter> hi lo]` of a dynamic value, packed as two big-endian bytes.
    pub fn frame_native(letter: u8, value: &LegacyValue) -> Result<[u8; 5], MotionError> {
        let bytes = native::to_bytes(value, 2, true)?;
        Ok([b'[', letter, bytes[0], bytes[1], b']'])
    }

    /// The same dynamic value for every letter.
    pub fn repeated_native(letters: &[u8], value: &LegacyValue) -> Result<Vec<u8>, MotionError> {
        let mut out = Vec::with_capacity(letters.len() * 5);
        for &letter in letters {
            out.extend_from_slice(&frame_native(letter, value).map_err(|e| e.in_frame(GEN_SMC3_COMMAND_TO_BYTES))?);
        }
        Ok(out)
    }

    /// The position commands on dynamic values: for a 10-bit box every value is first rescaled
    /// (one line per value, all of them before any packing), then each is packed into
    /// two big-endian bytes on a line of its own.
    pub fn positions_native(
        letters: &[u8],
        values: &[LegacyValue],
        resolution: Resolution,
    ) -> Result<Vec<u8>, MotionError> {
        let (function, scale_line, first_line) = if letters.contains(&b'Y') {
            ("gen_abcy_command", GEN_ABCY_FIRST_SCALE_LINE, GEN_ABCY_FIRST_LINE)
        } else {
            ("gen_abc_command", GEN_ABC_FIRST_SCALE_LINE, GEN_ABC_FIRST_LINE)
        };
        let mut sent = values.to_vec();
        if resolution.scales_to_ten_bit() {
            for (value, line) in sent.iter_mut().zip(scale_line..) {
                *value = to_ten_bit_native(value).map_err(|e| e.in_frame(at(function, line)))?;
            }
        }
        let mut out = Vec::with_capacity(letters.len() * 5);
        for ((&letter, value), line) in letters.iter().zip(&sent).zip(first_line..) {
            out.extend_from_slice(&frame_native(letter, value).map_err(|e| e.in_frame(at(function, line)))?);
        }
        Ok(out)
    }

    /// A dynamic value rescaled from 0..4095 to 0..1023, truncated.
    pub fn to_ten_bit_native(value: &LegacyValue) -> Result<LegacyValue, MotionError> {
        let mapped = native::map([value, &native::int(0), &native::int(4095), &native::int(0), &native::int(1023)])?;
        native::trunc_float(mapped)
    }

    /// The linear rescale from 0..4095 clamped to 0..=1023, truncated: the scaling step of the position commands.
    pub fn to_ten_bit(value: i64) -> i64 {
        let (in_min, in_max, out_min, out_max) = (0.0, 4095.0, 0.0, 1023.0);
        let mapped = (value as f64 - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
        mapped.clamp(out_min, out_max) as i64
    }

    /// First `[..]` frame in `data` whose command byte is `command` (any frame for `None`). For `v` a trailing
    /// `4]` (12-bit DOF Reality firmware) belongs to the frame.
    pub fn find_reply(data: &[u8], command: Option<u8>) -> Option<&[u8]> {
        let mut start = 0;
        loop {
            let open = start + data.get(start..)?.iter().position(|&b| b == b'[')?;
            let close = open + 1 + data[open + 1..].iter().position(|&b| b == b']')?;
            let mut end = close + 1;
            if command == Some(b'v') && data.get(end..end + 2) == Some(b"4]") {
                end += 2;
            }
            let frame = &data[open..end];
            if command.is_none_or(|c| frame.get(1) == Some(&c)) {
                return Some(frame);
            }
            start = end;
        }
    }

    /// Decodes a `[ver]` reply: the bytes after the command byte
    /// are a big-endian number read as a decimal string.
    pub fn decode_version(frame: &[u8]) -> VersionReply {
        let unknown = VersionReply { leading_digit: -1, version: -1 };
        let inner = frame.get(1..).unwrap_or_default();
        let inner = match inner.strip_suffix(b"]4]") {
            Some(stripped) => stripped,
            None => &inner[..inner.len().saturating_sub(1)],
        };
        let value = inner.get(1..).unwrap_or_default();
        let significant: &[u8] = match value.iter().position(|&b| b != 0) {
            Some(first) => &value[first..],
            None => &[],
        };
        // Three or more significant bytes are ≥ 65536, i.e. at least 5 decimal digits.
        let number = match significant {
            [] => 0,
            [lo] => u16::from(*lo),
            [hi, lo] => u16::from_be_bytes([*hi, *lo]),
            _ => return unknown,
        };
        let digits = number.to_string();
        let parse = |s: &str| s.parse::<i64>().expect("decimal digits");
        match digits.len() {
            4 => VersionReply { leading_digit: parse(&digits[..1]), version: parse(&digits[1..]) },
            3 => VersionReply { leading_digit: -1, version: i64::from(number) },
            _ => unknown,
        }
    }

    /// 12 when the `[ver]` frame ends with the `]4]` marker, else 10.
    pub fn reply_resolution(frame: &[u8]) -> Resolution {
        if frame.ends_with(b"]4]") {
            Resolution::TWELVE_BIT
        } else {
            Resolution::TEN_BIT
        }
    }

    /// The part of `output` after its last `\n`.
    pub fn last_line(output: &[u8]) -> &[u8] {
        output.rsplit(|&b| b == b'\n').next().unwrap_or(output)
    }

    /// Model code from a `[vmd]` reply: the second-to-last byte of a last line starting with `[vm`.
    pub fn model_reply(output: &[u8]) -> Option<i64> {
        let line = last_line(output);
        line.starts_with(b"[vm").then(|| i64::from(line[line.len() - 2]))
    }

    /// Box from a `[v6d]` reply: last line exactly `[v\0'1']` (box 1) or `[v\0'2']` (box 2).
    pub fn box_reply(output: &[u8]) -> Option<BoxSlot> {
        match last_line(output) {
            b"[v\x001]" => Some(BoxSlot::Left),
            b"[v\x002]" => Some(BoxSlot::Right),
            _ => None,
        }
    }

    /// Whether a `[rd..]` gain reply must be rewritten: [`first_three_differ`], or the reply holds a 4th frame
    /// whose low byte differs from `expected`. The connect uses it for kp and ki of both boxes.
    pub fn gains_need_update(reply: &[u8], expected: i64, label: &str) -> bool {
        gains_need_update_wide(reply, &num_bigint::BigInt::from(expected), label)
    }

    pub(super) fn gains_need_update_wide(reply: &[u8], expected: &num_bigint::BigInt, label: &str) -> bool {
        first_three_differ_wide(reply, expected, label)
            || reply.get(FOURTH_GAIN_POSITION).is_some_and(|&low| num_bigint::BigInt::from(low) != *expected)
    }

    /// Whether the low byte of one of the first three frames of a `[rd..]` reply differs from `expected`. A reply
    /// too short to hold all three is "unknown" and does not ask for an update.
    pub fn first_three_differ(reply: &[u8], expected: i64, label: &str) -> bool {
        first_three_differ_wide(reply, &num_bigint::BigInt::from(expected), label)
    }

    fn first_three_differ_wide(reply: &[u8], expected: &num_bigint::BigInt, label: &str) -> bool {
        let missing: Vec<usize> = GAIN_POSITIONS.into_iter().filter(|&p| reply.len() <= p).collect();
        if !missing.is_empty() {
            debug!(
                "SMC3 {label} response too short, skipping parameter update | len={} | missing_positions={missing:?} | output={reply:02x?}",
                reply.len()
            );
            return false;
        }
        GAIN_POSITIONS.into_iter().any(|p| num_bigint::BigInt::from(reply[p]) != *expected)
    }
}

/// SMC3 / DOF Reality settings, re-read on every call.
#[derive(Debug, Clone, PartialEq)]
pub struct Smc3Settings {
    /// Where the settings are read live (the refreshed raw values are the fallback).
    pub runtime: super::driver_support::RuntimeReader,
    /// The values taken at start. An invalid port is still an integer; the error of sending to it is handled later.
    pub native_startup: Option<Result<NativeSmc3Startup, String>>,
    pub native_switches: Option<NativeSmc3Switches>,
    /// The raw `MOTIONBOX.reaction_speed` (converted to an integer where it is used).
    pub native_reaction_speed: Option<NativeSetting>,
    /// `[MOTIONBOX] enable_debug`: fake connection, frames go to the UDP mirror only.
    pub enable_debug: bool,
    /// `[MOTIONBOX] reaction_speed`.
    pub reaction_speed: i64,
    /// `[DOF REALITY] update_kp` ("Auto Set KI and KP").
    pub update_kp: bool,
    /// `[DOF REALITY] left_controller_com_port`; with `right_controller_com_port`, skips the scan when set.
    pub left_controller_com_port: String,
    pub right_controller_com_port: String,
    /// `[DOF REALITY] left_controller_resolution`, as written in the config (parsed as an integer, 10 when
    /// that fails).
    pub left_controller_resolution: String,
    pub right_controller_resolution: String,
    /// `[DOF REALITY] left_debug_port`; read once, when the driver is created.
    pub left_debug_port: u16,
    pub right_debug_port: u16,
    /// Whether the pre-connection reset (open, flush, close,
    /// 1.5 s) runs before every probe. Off by default.
    pub pre_connection_reset: bool,
    /// Whether ports open exclusively (the default); without the lock
    /// a port a live object holds opens again.
    pub exclusive_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSmc3Startup {
    pub debug_ports: [Option<u16>; 2],
    pub previous_kp: BigInt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSmc3Switches {
    pub enable_debug: Result<bool, String>,
    pub update_kp: Result<bool, String>,
}

impl Smc3Settings {
    /// The reaction speed setting as an integer plus 20, at any width.
    fn kp(&self) -> Result<BigInt, MotionError> {
        let reaction = match &self.native_reaction_speed {
            Some(native) => native::to_bigint(
                &self.runtime.value("MOTIONBOX", "reaction_speed", native).map_err(MotionError::Native)?,
            )?,
            None => BigInt::from(self.reaction_speed),
        };
        Ok(reaction + KP_OFFSET)
    }

    fn debug_enabled(&self) -> Result<bool, MotionError> {
        self.native_switches
            .as_ref()
            .map_or(Ok(self.enable_debug), |native| {
                self.runtime.switch("MOTIONBOX", "enable_debug", &native.enable_debug)
            })
            .map_err(MotionError::Native)
    }

    fn gains_enabled(&self) -> Result<bool, MotionError> {
        self.native_switches
            .as_ref()
            .map_or(Ok(self.update_kp), |native| self.runtime.switch("DOF REALITY", "update_kp", &native.update_kp))
            .map_err(MotionError::Native)
    }
}

impl Default for Smc3Settings {
    fn default() -> Self {
        Smc3Settings {
            runtime: Default::default(),
            native_startup: None,
            native_switches: None,
            native_reaction_speed: None,
            enable_debug: false,
            reaction_speed: 100,
            update_kp: true,
            left_controller_com_port: String::new(),
            right_controller_com_port: String::new(),
            left_controller_resolution: "10".into(),
            right_controller_resolution: "10".into(),
            left_debug_port: DEFAULT_LEFT_DEBUG_PORT,
            right_debug_port: DEFAULT_RIGHT_DEBUG_PORT,
            pre_connection_reset: false,
            exclusive_open: true,
        }
    }
}

/// A serial port object. Several holders can refer to the same object (a slot of the controller list and
/// the latest probe of a box); closing closes the port but the object stays referenced,
/// and the port is also closed when the last reference goes away.
struct SerialObject {
    port: String,
    /// `None` after the port was closed.
    link: Option<Box<dyn Link>>,
    /// Unparsed feedback replies (diagnostics only).
    inbox: Vec<u8>,
}

impl SerialObject {
    /// Any I/O on a closed port is the port-not-open error.
    fn link(&mut self) -> Result<&mut (dyn Link + 'static), LinkError> {
        self.link.as_deref_mut().ok_or_else(|| {
            LinkError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Attempting to use a port that is not open",
            ))
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        self.link()?.write_all(bytes)
    }

    /// Write, wait [`COMMAND_PAUSE`], return what arrived.
    fn query(&mut self, command: &[u8], clock: &mut dyn Clock) -> Result<Vec<u8>, LinkError> {
        self.write(command)?;
        clock.sleep(COMMAND_PAUSE);
        self.link()?.read_available()
    }

    /// Write, then wait [`COMMAND_PAUSE`].
    fn configure(&mut self, bytes: &[u8], clock: &mut dyn Clock) -> Result<(), LinkError> {
        self.write(bytes)?;
        clock.sleep(COMMAND_PAUSE);
        Ok(())
    }

    fn close(&mut self) {
        self.link = None;
    }

    fn is_open(&self) -> bool {
        self.link.is_some()
    }
}

/// A shared reference to a [`SerialObject`].
#[derive(Clone)]
struct Handle(Arc<Mutex<SerialObject>>);

impl Handle {
    fn new(port: &str, link: Box<dyn Link>) -> Self {
        Handle(Arc::new(Mutex::new(SerialObject { port: port.to_string(), link: Some(link), inbox: Vec::new() })))
    }

    fn get(&self) -> MutexGuard<'_, SerialObject> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn port(&self) -> String {
        self.get().port.clone()
    }
}

/// Which probe a decoded `[ver]` reply came from: the scan's first box or the second-box search. Each
/// updates its own frame resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeRole {
    First,
    Second,
}

impl ProbeRole {
    fn frame_index(self) -> usize {
        match self {
            ProbeRole::First => 0,
            ProbeRole::Second => 1,
        }
    }

    /// The holder the probe stores its object in (the latest first-box or second-box probe).
    fn global_name(self) -> &'static str {
        match self {
            ProbeRole::First => "first_controller",
            ProbeRole::Second => "second_controller",
        }
    }

    /// The word the second box's open-failure lines insert (`'SMC3 secondary open failed on ...'`).
    fn open_label(self) -> &'static str {
        match self {
            ProbeRole::First => "",
            ProbeRole::Second => "secondary ",
        }
    }
}

/// Outcome of the `[ver]` probe of one port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Accepted(VersionReply),
    /// Decoded version -1: "a smc3, but not the DOF Reality".
    NotDofReality,
    /// No usable reply, a version below 100, or an I/O error: "not the smc3".
    NotSmc3,
}

impl Verdict {
    fn of(reply: VersionReply) -> Verdict {
        if reply.version >= MIN_VERSION {
            Verdict::Accepted(reply)
        } else if reply.version == -1 {
            Verdict::NotDofReality
        } else {
            Verdict::NotSmc3
        }
    }
}

/// Read replies until a `[v..]` frame arrives, sending `[ver]` 1 s after opening and every 2 s after that
/// (flushing input first), for at most 10 s; give up early on more than 128 bytes without `[`.
/// The first box's loop logs each step with its time since
/// the open (`[smc3_ver] T+<seconds>`, two decimals); the second box's loop logs
/// nothing.
fn read_version_reply(
    link: &mut dyn Link,
    clock: &mut dyn Clock,
    role: ProbeRole,
) -> Result<Option<Vec<u8>>, LinkError> {
    let log = role == ProbeRole::First;
    let opened = clock.now();
    let elapsed = |clock: &dyn Clock| py::round2((clock.now() - opened).as_secs_f64());
    let deadline = opened + PROBE_WINDOW;
    let mut output = Vec::new();
    let mut last_sent: Option<Duration> = None;
    let mut sends = 0u32;
    while clock.now() < deadline {
        let chunk = link.read_available()?;
        if !chunk.is_empty() {
            if log {
                let more = match chunk.len().checked_sub(64) {
                    Some(extra) if extra > 0 => format!(" ...(+{extra} more bytes)"),
                    _ => String::new(),
                };
                debug!("[smc3_ver] T+{} rx: {}{more}", elapsed(clock), py::hex(&chunk[..chunk.len().min(64)]));
            }
            output.extend_from_slice(&chunk);
            if let Some(frame) = wire::find_reply(&output, Some(b'v')) {
                if log {
                    debug!("[smc3_ver] T+{} frame found: {}", elapsed(clock), py::hex(frame));
                }
                break;
            }
            if !output.contains(&b'[') && output.len() > MAX_UNFRAMED_BYTES {
                if log {
                    debug!(
                        "[smc3_ver] T+{} received {} bytes with no '[' framing, not an SMC3, abandoning port",
                        elapsed(clock),
                        output.len()
                    );
                }
                break;
            }
        }
        let now = clock.now();
        if last_sent.is_none_or(|t| now - t >= RESEND_INTERVAL) && now - opened >= BOOT_DELAY {
            link.clear_input()?;
            output.clear();
            link.write_all(b"[ver]")?;
            sends += 1;
            last_sent = Some(clock.now());
            if log {
                debug!("[smc3_ver] T+{} sent [ver] #{sends}", elapsed(clock));
            }
        }
        clock.sleep(POLL_INTERVAL);
    }
    if log {
        debug!("[smc3_ver] loop done T+{} ver_sends={sends} output={}", elapsed(clock), py::bytes_repr(&output));
    }
    Ok(wire::find_reply(&output, Some(b'v')).map(<[u8]>::to_vec))
}

/// Resolution of a manually configured port: the raw setting as an integer; any failure
/// of the lookup or the conversion is 10; an
/// integer beyond `i64` keeps its sign at the `i64` limit, which compares unequal to 10 as the integer does.
fn native_configured_resolution(setting: &Result<LegacyValue, String>) -> Resolution {
    let Ok(value) = setting else { return Resolution(10) };
    match value.py_integer() {
        Ok(integer) => Resolution(
            native::as_i64(&native::big(integer.clone())).unwrap_or(if integer.sign() == num_bigint::Sign::Minus {
                i64::MIN
            } else {
                i64::MAX
            }),
        ),
        Err(_) => Resolution(10),
    }
}

/// The DOF Reality / SMC3 driver: up to two boxes.
///
/// The controller list (box 1 / box 2) and the two latest-probe holders
/// refer to shared serial objects. A new connect keeps the objects of
/// an earlier connection; reopening a port that one of them still holds fails
/// because ports are opened exclusively, and the error paths then close
/// the object they name.
pub struct Smc3Driver {
    settings: LiveSettings<Smc3Settings>,
    /// UDP mirror targets of the box-1 and box-2 frames.
    debug_targets: [Option<SocketAddr>; 2],
    /// The controller list: index 0 = box 1, 1 = box 2.
    controller: [Option<Handle>; 2],
    /// The object of the latest first-box probe.
    first_controller: Option<Handle>,
    /// The object of the latest second-box probe.
    second_controller: Option<Handle>,
    /// Scaling of the box-1 and box-2 frames.
    frame_resolution: [Resolution; 2],
    /// Kp of the previous frame.
    previous_kp: BigInt,
    identity: ControllerIdentity,
    actuators_position: Option<ActuatorPositions>,
}

impl Smc3Driver {
    /// Create the driver; the debug ports and the starting Kp come from the current settings.
    pub fn new(settings: LiveSettings<Smc3Settings>) -> Self {
        let initial = settings.get();
        Self::starting_from(settings, &initial)
    }

    /// Registry factory. The debug ports and the starting Kp are taken now, at registration;
    /// the driver itself is created on first use.
    pub fn factory(settings: LiveSettings<Smc3Settings>) -> DriverFactory {
        let at_start = settings.get();
        Box::new(move || Box::new(Smc3Driver::starting_from(settings.clone(), &at_start)))
    }

    /// The driver's starting state: the two debug targets, the starting Kp,
    /// an empty controller list and resolutions of 10.
    fn starting_from(settings: LiveSettings<Smc3Settings>, initial: &Smc3Settings) -> Self {
        let debug_target = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let startup = initial.native_startup.as_ref().and_then(|startup| startup.as_ref().ok());
        Smc3Driver {
            debug_targets: startup.map_or_else(
                || [Some(debug_target(initial.left_debug_port)), Some(debug_target(initial.right_debug_port))],
                |startup| startup.debug_ports.map(|port| port.map(debug_target)),
            ),
            settings,
            controller: [None, None],
            first_controller: None,
            second_controller: None,
            frame_resolution: [Resolution::TEN_BIT; 2],
            previous_kp: startup
                .map_or_else(|| BigInt::from(initial.reaction_speed) + KP_OFFSET, |s| s.previous_kp.clone()),
            identity: ControllerIdentity::default(),
            actuators_position: None,
        }
    }

    /// Port of the object in slot `slot` of the controller list (open or closed), if any.
    pub fn port(&self, slot: BoxSlot) -> Option<String> {
        self.controller[slot.index()].as_ref().map(Handle::port)
    }

    /// Whether slot `slot` of the controller list holds an object whose port is open.
    pub fn is_open(&self, slot: BoxSlot) -> bool {
        self.controller[slot.index()].as_ref().is_some_and(|h| h.get().is_open())
    }

    /// Frame scaling currently used for the box-1 and box-2 frames.
    pub fn frame_resolution(&self) -> [Resolution; 2] {
        self.frame_resolution
    }

    /// The debug-mode branch of a connect.
    fn connect_debug(ctx: &mut MotionContext<'_>) {
        ctx.link.status = LinkStatus::Connected;
        ctx.link.ports.insert(0, "COMX".into());
        if ctx.model.motors > 3 {
            ctx.link.ports.insert(1, "COMY".into());
        }
    }

    /// Open the configured ports without probing. Returns `false` when no port is configured.
    /// Each port is opened at 500000 baud into its slot of the controller list; when that fails,
    /// the slot's motion port entry is removed, the slot's object (of an earlier connection, if any) is closed
    /// (errors ignored) and the connect ends. Afterwards every filled slot reports the configured port.
    fn open_configured_ports(&mut self, ctx: &mut MotionContext<'_>) -> Result<bool, MotionError> {
        let live = &self.settings;
        // Each access reads the `DOF REALITY` setting as it is now (the live tree when installed).
        let raw_port = |slot: BoxSlot| -> Result<LegacyValue, MotionError> {
            live.read(|s| {
                let (key, typed) = match slot {
                    BoxSlot::Left => ("left_controller_com_port", &s.left_controller_com_port),
                    BoxSlot::Right => ("right_controller_com_port", &s.right_controller_com_port),
                };
                s.runtime.value("DOF REALITY", key, &Ok(LegacyValue::Str(typed.clone()))).map_err(MotionError::Native)
            })
        };
        let is_set =
            |slot: BoxSlot| -> Result<bool, MotionError> { Ok(!native::eq(&raw_port(slot)?, &LegacyValue::from(""))?) };
        let port_of = |slot: BoxSlot| -> Result<String, MotionError> {
            let raw = raw_port(slot)?;
            Ok(raw.python_string().and_then(|text| text.scalar_string()).unwrap_or_else(|| native::str_of(&raw)))
        };
        let resolution_of = |slot: BoxSlot| {
            live.read(|s| {
                let (key, typed) = match slot {
                    BoxSlot::Left => ("left_controller_resolution", &s.left_controller_resolution),
                    BoxSlot::Right => ("right_controller_resolution", &s.right_controller_resolution),
                };
                native_configured_resolution(&s.runtime.value("DOF REALITY", key, &Ok(LegacyValue::Str(typed.clone()))))
            })
        };
        // Either port may be set (the right one is only read when the left one is empty).
        if !is_set(BoxSlot::Left)? && !is_set(BoxSlot::Right)? {
            return Ok(false);
        }
        let options = OpenOptions { baud_rate: BAUD_RATE, write_timeout: None };
        for slot in BoxSlot::BOTH {
            // The test whether the port is set is not handled; the open is: the port setting is read again
            // for the open at 500000 baud, and the resolution setting becomes 10 when it is not an integer.
            // A failed port read in the open takes the same path as a failed open, for both boxes.
            if !is_set(slot)? {
                continue;
            }
            let opened = port_of(slot).and_then(|port| {
                let exclusive = live.read(|s| s.exclusive_open);
                open_port(ctx.host, &port, &options, exclusive).map(|link| (port, link)).map_err(MotionError::from)
            });
            match opened {
                Ok((port, link)) => {
                    self.controller[slot.index()] = Some(Handle::new(&port, link));
                    self.frame_resolution[slot.index()] = resolution_of(slot);
                }
                Err(handled) => {
                    // The handling reads the port again for its message; an error there occurs while the first one
                    // is being handled.
                    let port = port_of(slot).map_err(|e| e.while_handling(handled))?;
                    debug!("Cannot open connection on the {port} port");
                    ctx.link.ports.remove(&slot.index());
                    if let Some(held) = &self.controller[slot.index()] {
                        held.get().close();
                    }
                    return Ok(true);
                }
            }
        }
        // Connected when any slot holds an object.
        if self.controller.iter().any(Option::is_some) {
            ctx.link.status = LinkStatus::Connected;
            for slot in BoxSlot::BOTH {
                if self.controller[slot.index()].is_some() {
                    ctx.link.ports.insert(slot.index(), port_of(slot)?);
                }
            }
        }
        Ok(true)
    }

    /// Scan every port for the box(es).
    fn scan(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let ports = ctx.host.serial_ports_checked()?;
        log_scan_start(&ports, ctx);
        for port in &ports {
            if !may_probe(&port.path, ctx.host) {
                continue;
            }
            let Some(first) = self.open_for_probe(port, ProbeRole::First, ctx.host) else {
                continue;
            };
            match self.probe(&first, ProbeRole::First, ctx.host) {
                Verdict::Accepted(reply) => {
                    if self.adopt_first_box(first, reply, ctx)? {
                        return Ok(());
                    }
                }
                verdict => {
                    log_rejection(verdict, ProbeRole::First);
                    first.get().close();
                }
            }
        }
        ctx.link.status = LinkStatus::NotConnectedAfterWriteError;
        Ok(())
    }

    /// Pre-connection reset (when enabled), then open `port` for probing and store it as
    /// the latest probe of its box. When the open fails, the object that holder still names (the
    /// previous probe's) is closed (the second box logs neither the
    /// port nor the open).
    fn open_for_probe(&mut self, port: &PortInfo, role: ProbeRole, host: &mut dyn MotionHost) -> Option<Handle> {
        let path = port.path.as_str();
        let (desc, hwid) = port_desc_hwid(port, host);
        if self.settings.read(|s| s.pre_connection_reset) {
            pre_reset(path, role, &self.settings, host);
        }
        if role == ProbeRole::First {
            debug!("trying to connect to: {path}");
            debug!("{path} {desc} {hwid}");
        }
        let options = OpenOptions { baud_rate: BAUD_RATE, write_timeout: Some(WRITE_TIMEOUT) };
        let exclusive = self.settings.read(|s| s.exclusive_open);
        let global = match role {
            ProbeRole::First => &mut self.first_controller,
            ProbeRole::Second => &mut self.second_controller,
        };
        match open_port(host, path, &options, exclusive) {
            Ok(link) => {
                if role == ProbeRole::First {
                    debug!("[smc3_ver] port opened: {path}");
                }
                let handle = Handle::new(path, link);
                *global = Some(handle.clone());
                Some(handle)
            }
            Err(e) => {
                let (repr, _, errno) = open_exception(path, &e);
                debug!(
                    "SMC3 {}open failed on {path} | desc={desc} | hwid={hwid} | error={repr} | errno={} | winerror=None",
                    role.open_label(),
                    or_none(errno)
                );
                match global {
                    Some(previous) => previous.get().close(),
                    None => debug!(
                        "SMC3 {}close after open failure failed on {path} | error=NameError(\"name '{}' is not defined\")",
                        role.open_label(),
                        role.global_name()
                    ),
                }
                None
            }
        }
    }

    /// `[ver]` probe; a decoded reply also sets the frame resolution of `role`. A failed probe closes the
    /// object (for the second box nothing is logged).
    fn probe(&mut self, open: &Handle, role: ProbeRole, host: &mut dyn MotionHost) -> Verdict {
        let mut object = open.get();
        let reply = object.link().and_then(|link| read_version_reply(link, host.clock(), role));
        // The message of the probe's error: the decoding error when no frame arrived,
        // else the port error.
        let failure = match reply {
            Ok(Some(frame)) => {
                let resolution = wire::reply_resolution(&frame);
                self.frame_resolution[role.frame_index()] = resolution;
                let name = match role {
                    ProbeRole::First => "first_controller_resolution",
                    ProbeRole::Second => "second_controller_resolution",
                };
                debug!("{name}: {}, output[ver]: {}", resolution.0, py::bytes_repr(&frame));
                return Verdict::of(wire::decode_version(&frame));
            }
            Ok(None) => "'bool' object is not subscriptable".to_string(),
            Err(e) => exception_text(&MotionError::from(e)),
        };
        if role == ProbeRole::First {
            debug!("[smc3_ver] exception: {failure}");
        }
        object.close();
        Verdict::NotSmc3
    }

    /// Identify an accepted first box, update its gains and place it; for two-box models also find the other
    /// box. Returns whether the platform is now connected.
    fn adopt_first_box(
        &mut self,
        first: Handle,
        reply: VersionReply,
        ctx: &mut MotionContext<'_>,
    ) -> Result<bool, MotionError> {
        let port = first.port();
        self.identity.version = Some(reply.version);
        debug!("SMC3 found on {port} port");
        let model_output = first.get().query(b"[vmd]", ctx.host.clock())?;
        let Some(model_id) = wire::model_reply(&model_output) else {
            self.identity.model_id = Some(ctx.model.id);
            self.identity.reported_motors = Some(ctx.model.motors as i64);
            self.place(BoxSlot::Left, first, ctx);
            ctx.link.status = LinkStatus::Connected;
            return Ok(true);
        };
        self.identity.model_id = Some(model_id);
        self.identity.reported_motors = Some(reply.leading_digit);
        log_version_text(&mut first.get(), ctx.host.clock());
        // The `update_kp` switch as a boolean, read now.
        if self.settings.read(Smc3Settings::gains_enabled).map_err(switch_error(at("connect", 410)))? {
            update_gains(&mut first.get(), &self.settings, ProbeRole::First, ctx.host.clock())?;
        }
        if ctx.model.motors != TWO_BOX_MOTORS {
            self.place(BoxSlot::Left, first, ctx);
            ctx.link.status = LinkStatus::Connected;
            return Ok(true);
        }
        let box_output = first.get().query(b"[v6d]", ctx.host.clock())?;
        let Some(slot) = wire::box_reply(&box_output) else {
            // Neither box 1 nor box 2; the scan goes on with the next port. The object stays
            // the latest first-box probe (open) until the next first-box probe replaces it.
            return Ok(false);
        };
        self.place(slot, first, ctx);
        self.find_second_box(&port, ctx)
    }

    /// Probe the other ports for the second box of a two-box platform; a box with an unknown number does not
    /// count and the search goes on.
    fn find_second_box(&mut self, first_port: &str, ctx: &mut MotionContext<'_>) -> Result<bool, MotionError> {
        for port in ctx.host.serial_ports_checked()? {
            if !may_probe(&port.path, ctx.host) || port.path == first_port {
                continue;
            }
            let Some(second) = self.open_for_probe(&port, ProbeRole::Second, ctx.host) else {
                continue;
            };
            match self.probe(&second, ProbeRole::Second, ctx.host) {
                Verdict::Accepted(_) => {
                    let box_output = second.get().query(b"[v6d]", ctx.host.clock())?;
                    // The `update_kp` switch as a boolean, read now.
                    if self.settings.read(Smc3Settings::gains_enabled).map_err(switch_error(at("connect", 567)))? {
                        update_gains(&mut second.get(), &self.settings, ProbeRole::Second, ctx.host.clock())?;
                    }
                    let Some(slot) = wire::box_reply(&box_output) else {
                        continue;
                    };
                    self.place(slot, second, ctx);
                    ctx.link.status = LinkStatus::Connected;
                    return Ok(true);
                }
                verdict => {
                    log_rejection(verdict, ProbeRole::Second);
                    second.get().close();
                }
            }
        }
        Ok(false)
    }

    /// Stores the object in its slot of the controller list and its port in the motion ports.
    fn place(&mut self, slot: BoxSlot, open: Handle, ctx: &mut MotionContext<'_>) {
        ctx.link.ports.insert(slot.index(), open.port());
        self.controller[slot.index()] = Some(open);
    }

    /// Box-1 frame `[A m0][B m1][C m2][Y m3]` and box-2 frame `[A m3][B m4][C m5]`, built at
    /// the two frame lines `lines` of `function`, each with its box's resolution.
    /// Each frame reads its slots in order before it is built; the first one is complete (or has failed)
    /// before the second reads the list.
    fn frames(
        &self,
        motors: &LegacyValue,
        function: &'static str,
        lines: [u32; 2],
    ) -> Result<[Vec<u8>; 2], MotionError> {
        let build = |letters: &[u8], range: std::ops::Range<usize>, resolution, line| {
            let frame = at(function, line);
            let values = range.map(|index| native::item(motors, index)).collect::<Result<Vec<_>, _>>();
            let values = values.map_err(|e| e.in_frame(frame))?;
            wire::positions_native(letters, &values, resolution).map_err(|e| e.in_frame(frame))
        };
        Ok([
            build(b"ABCY", 0..4, self.frame_resolution[0], lines[0])?,
            build(b"ABC", 3..6, self.frame_resolution[1], lines[1])?,
        ])
    }

    /// The UDP mirror on a frame of `i64`s; see [`debug_native`](Self::debug_native).
    pub fn debug(&self, motors: &[i64], host: &mut dyn MotionHost) -> Result<(), MotionError> {
        self.debug_native(&native::list_of(motors), host)
    }

    /// The UDP mirror: both frames as single datagrams to the two debug targets; a failed send is
    /// handled and changes nothing. Stream and test frames
    /// call it in debug mode, and the registry calls it on every frame while the Telemetry
    /// page's debug status is on.
    pub fn debug_native(&self, motors: &LegacyValue, host: &mut dyn MotionHost) -> Result<(), MotionError> {
        // The two frames, at their frame lines.
        let frames = self.frames(motors, "debug", [899, 900])?;
        for ((socket, target), frame) in MIRROR_SOCKETS.iter().zip(&self.debug_targets).zip(&frames) {
            if let Some(target) = target {
                let _ = host.send_datagram_from(*socket, *target, frame);
            }
        }
        Ok(())
    }

    /// Shared body of a stream frame and a test frame. A stream frame publishes the box-1 positions before the
    /// box-1 write; a test frame only after that write succeeded.
    fn stream(
        &mut self,
        motors: &LegacyValue,
        ctx: &mut MotionContext<'_>,
        on_failure: FailurePolicy,
    ) -> Result<(), MotionError> {
        let lines = on_failure.lines();
        let function = lines.function;
        // The debug switch as a boolean.
        if self.settings.read(Smc3Settings::debug_enabled).map_err(switch_error(at(function, lines.enable_debug)))? {
            // The UDP mirror of the frame.
            return self.debug_native(motors, ctx.host).map_err(|e| e.through(at(function, lines.debug_call)));
        }
        // A stream frame reads Kp (the integer reaction speed plus 20) before it builds the frames;
        // a test frame builds them first and reads Kp after.
        let read_kp = |settings: &LiveSettings<Smc3Settings>| {
            settings.read(Smc3Settings::kp).map_err(|e| e.in_frame(at(function, lines.reaction_speed)))
        };
        let (kp, frames) = match on_failure {
            FailurePolicy::KeepOpen => {
                let kp = read_kp(&self.settings)?;
                (kp, self.frames(motors, function, lines.frames)?)
            }
            FailurePolicy::CloseAll => {
                let frames = self.frames(motors, function, lines.frames)?;
                (read_kp(&self.settings)?, frames)
            }
        };
        let gain = native::big(kp.clone());
        for slot in BoxSlot::BOTH {
            let index = slot.index();
            let publishes = slot == BoxSlot::Left;
            let Some(open) = self.controller[index].clone() else {
                continue;
            };
            let settings = &self.settings;
            let previous_kp = self.previous_kp.clone();
            if publishes && on_failure == FailurePolicy::KeepOpen {
                // The published positions read their slots again when they are built.
                self.actuators_position = Some(commanded_positions(motors, at(function, lines.positions))?);
            }
            let mut object = open.get();
            let sent = object.write(&frames[index]).map_err(MotionError::from).and_then(|()| {
                if publishes && on_failure == FailurePolicy::CloseAll {
                    // After the box-1 write; its errors are handled with the write's.
                    self.actuators_position = Some(commanded_positions(motors, at(function, lines.positions))?);
                }
                // The `update_kp` switch as a boolean, read for each box.
                if settings.read(Smc3Settings::gains_enabled)? && previous_kp != kp {
                    push_gain(&mut object, &gain, ctx.host.clock())
                } else {
                    Ok(())
                }
            });
            if let Err(e) = sent {
                match on_failure {
                    // The failure is logged at debug level; a serial error has no error number
                    // to report, so both number fields print `None`.
                    FailurePolicy::KeepOpen => debug!(
                        "SMC3 send_stream failed on controller[{index}] | port={} | is_open={} | error={} | \
                         errno=None | winerror=None",
                        object.port,
                        if object.is_open() { "True" } else { "False" },
                        exception_repr(&e),
                    ),
                    // Nothing is printed for box 1; box 2 prints 'error sending test2' with the message.
                    FailurePolicy::CloseAll => {
                        if slot == BoxSlot::Right {
                            python_print(&["error sending test2", &exception_text(&e)]);
                        }
                    }
                }
                ctx.link.status = LinkStatus::NotConnectedAfterWriteError;
                if on_failure == FailurePolicy::CloseAll {
                    object.close();
                    self.controller = [None, None];
                }
            }
        }
        self.previous_kp = kp;
        Ok(())
    }
}

/// How a stream frame and a test frame differ: what a failed frame write does besides setting `NotConnected`, and
/// whether the actuator positions are published before or after the box-1 write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailurePolicy {
    /// A stream frame: keep the ports open; publish before the write.
    KeepOpen,
    /// A test frame: close the box and forget both; publish after a successful write.
    CloseAll,
}

/// The frame lines of the steps of a stream or test frame whose errors leave it.
struct StreamLines {
    function: &'static str,
    /// The read of the debug switch.
    enable_debug: u32,
    /// The UDP mirror.
    debug_call: u32,
    /// The read of Kp.
    reaction_speed: u32,
    /// The two position frames.
    frames: [u32; 2],
    /// The published actuator positions.
    positions: u32,
}

impl FailurePolicy {
    fn lines(self) -> StreamLines {
        match self {
            FailurePolicy::KeepOpen => StreamLines {
                function: "send_stream",
                enable_debug: 708,
                debug_call: 709,
                reaction_speed: 714,
                frames: [715, 716],
                positions: 721,
            },
            FailurePolicy::CloseAll => StreamLines {
                function: "send_test",
                enable_debug: 793,
                debug_call: 794,
                reaction_speed: 800,
                frames: [797, 798],
                positions: 807,
            },
        }
    }
}

/// The message alone of a failed frame write's error.
fn exception_text(e: &MotionError) -> String {
    let e = e.root();
    match e {
        MotionError::Link(LinkError::Io(io)) => io.to_string(),
        MotionError::Link(LinkError::Port(port)) => port.description.clone(),
        MotionError::Native(_) => e.python_str(),
        other => other.to_string(),
    }
}

/// The class-and-message form of that error: `SerialTimeoutException('Write timeout')` for a write that did
/// not finish in time, `PortNotOpenError(...)` for a closed object, `SerialException(...)` for any other port
/// error.
fn exception_repr(e: &MotionError) -> String {
    let e = e.root();
    let text = exception_text(e);
    if let MotionError::Native(detail) = e {
        if let Some((class, _)) = detail.split_once(": ") {
            if class == "KeyError" {
                return format!("{class}({text})");
            }
            return format!("{class}({})", super::python::str_repr(&text));
        }
    }
    let class = match e {
        MotionError::Link(LinkError::Io(io)) if io.kind() == std::io::ErrorKind::TimedOut => "SerialTimeoutException",
        MotionError::Link(LinkError::Io(io)) if io.kind() == std::io::ErrorKind::NotConnected => "PortNotOpenError",
        MotionError::Link(_) => "SerialException",
        _ => return text,
    };
    format!("{class}('{text}')")
}

fn may_probe(path: &str, host: &dyn MotionHost) -> bool {
    scan_may_probe(path, &host.allowed_com_ports(), &host.claimed_ports())
}

/// The text forms of the values the SMC3 driver logs.
mod py {
    /// The quoted form of text: single quotes unless the text holds a `'` and no `"`.
    pub fn str_repr(text: &str) -> String {
        let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
        let mut out = String::with_capacity(text.len() + 2);
        out.push(quote);
        for c in text.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c == quote => {
                    out.push('\\');
                    out.push(c);
                }
                c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push(quote);
        out
    }

    /// The printed form of a byte string (`b'…'`).
    pub fn bytes_repr(data: &[u8]) -> String {
        let quote = if data.contains(&b'\'') && !data.contains(&b'"') { b'"' } else { b'\'' };
        let mut out = String::with_capacity(data.len() + 3);
        out.push('b');
        out.push(char::from(quote));
        for &b in data {
            match b {
                b'\\' => out.push_str("\\\\"),
                b'\n' => out.push_str("\\n"),
                b'\r' => out.push_str("\\r"),
                b'\t' => out.push_str("\\t"),
                b if b == quote => {
                    out.push('\\');
                    out.push(char::from(b));
                }
                0x20..=0x7e => out.push(char::from(b)),
                b => out.push_str(&format!("\\x{b:02x}")),
            }
        }
        out.push(char::from(quote));
        out
    }

    /// Lower-case hexadecimal of the bytes.
    pub fn hex(data: &[u8]) -> String {
        data.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The seconds correctly rounded to two decimals, printed as a float ([`float`]).
    pub fn round2(seconds: f64) -> String {
        let rounded: f64 = format!("{seconds:.2}").parse().unwrap_or(seconds);
        float(rounded)
    }

    /// The text of a float: the shortest text that reads back the same value, with `.0` for whole numbers.
    pub fn float(value: f64) -> String {
        if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e16 {
            format!("{value:.1}")
        } else {
            format!("{value}")
        }
    }
}

/// The description and hardware id of the listing entry of `port`: the USB
/// interface/product description, location and subsystem-specific hardware identity.
fn port_desc_hwid(port: &PortInfo, host: &mut dyn MotionHost) -> (String, String) {
    let details = host.port_details(port);
    (details.description.clone(), details.hwid_text().to_owned())
}

/// The motion status as text.
fn status_text(status: LinkStatus) -> &'static str {
    match status {
        LinkStatus::Connected => "Connected",
        LinkStatus::NotConnected => "Not connected",
        LinkStatus::NotConnectedAfterWriteError => "Not Connected",
    }
}

/// The "SMC3 scan start" debug line of a connect: the `(device, description, hwid)`
/// tuples of the listed ports, the ports in use, the motion ports and
/// the motion status.
fn log_scan_start(ports: &[PortInfo], ctx: &mut MotionContext<'_>) {
    let listed: Vec<String> = ports
        .iter()
        .map(|port| {
            let details = ctx.host.port_details(port);
            let hwid = details.hwid.as_deref().map(py::str_repr).unwrap_or_else(|| "None".into());
            format!("({}, {}, {})", py::str_repr(&port.path), py::str_repr(&details.description), hwid)
        })
        .collect();
    let claimed = ctx.host.claimed_ports();
    let in_use: Vec<String> = [
        &claimed.motion_compensation,
        &claimed.belt,
        &claimed.hapticr,
        &claimed.intellibox,
        &claimed.iflag,
        &claimed.sim3d,
    ]
    .into_iter()
    .map(|port| py::str_repr(port.as_deref().unwrap_or("")))
    .collect();
    let com_ports: Vec<String> =
        ctx.link.ports.iter().map(|(slot, port)| format!("{slot}: {}", py::str_repr(port))).collect();
    debug!(
        "SMC3 scan start | ports=[{}] | ports_in_use=[{}] | motion_com_port={{{}}} | motion_status={}",
        listed.join(", "),
        in_use.join(", "),
        com_ports.join(", "),
        status_text(ctx.link.status)
    );
}

/// The error number of an OS error text (`strerror`), as a serial open error carries it.
fn errno_of(description: &str) -> Option<i32> {
    (1..=133).find(|&n| {
        let text = std::io::Error::from_raw_os_error(n).to_string();
        text.strip_suffix(&format!(" (os error {n})")).unwrap_or(&text) == description
    })
}

/// The error of a port that cannot be opened: `SerialException(errno,
/// "could not open port <path>: [Errno <n>] <strerror>: '<path>'")`. Returns its class-and-message form, its
/// message and its error number.
fn open_exception(path: &str, error: &LinkError) -> (String, String, Option<i32>) {
    let description = match error {
        LinkError::Open { source, .. } | LinkError::Port(source) => source.description.clone(),
        LinkError::Io(io) => {
            let text = io.to_string();
            match io.raw_os_error() {
                Some(n) => text.strip_suffix(&format!(" (os error {n})")).unwrap_or(&text).to_string(),
                None => text,
            }
        }
    };
    let errno = match error {
        LinkError::Io(io) => io.raw_os_error().or_else(|| errno_of(&description)),
        _ => errno_of(&description),
    };
    match errno {
        Some(n) => {
            let message = format!("could not open port {path}: [Errno {n}] {description}: {}", py::str_repr(path));
            (format!("SerialException({n}, {})", py::str_repr(&message)), format!("[Errno {n}] {message}"), Some(n))
        }
        None => {
            let message = format!("could not open port {path}: {description}");
            (format!("SerialException({})", py::str_repr(&message)), message, None)
        }
    }
}

/// The text of an optional value (`None` when absent).
fn or_none(value: Option<i32>) -> String {
    value.map_or_else(|| "None".to_string(), |n| n.to_string())
}

/// The rejection branches after a `[ver]` probe (of either box):
/// the reason, `'Trying to disconnect'` (not in the second box's "not the DOF Reality" branch), then
/// the close, which never fails here.
fn log_rejection(verdict: Verdict, role: ProbeRole) {
    match verdict {
        Verdict::NotDofReality => {
            debug!("This is a smc3, but it is not the DOF Reality disconnecting and trying next port");
            if role == ProbeRole::First {
                debug!("Trying to disconnect");
            }
        }
        _ => {
            debug!("This is not the smc3, disconnecting and trying next port");
            debug!("Trying to disconnect");
        }
    }
}

/// Open at 500000 baud, flush input, close, then wait 1.5 s: the pre-connection reset.
/// The first box logs it and its failure, the second box is silent.
fn pre_reset(path: &str, role: ProbeRole, settings: &LiveSettings<Smc3Settings>, host: &mut dyn MotionHost) {
    let first = role == ProbeRole::First;
    if first {
        debug!("Linux detected: Pre-connection reset for {path}");
    }
    let options = OpenOptions { baud_rate: BAUD_RATE, write_timeout: None };
    let exclusive = settings.read(|s| s.exclusive_open);
    let flushed = open_port(host, path, &options, exclusive)
        .map_err(|e| open_exception(path, &e).1)
        .and_then(|mut link| link.clear_input().map_err(|e| exception_text(&MotionError::from(e))));
    match flushed {
        Ok(()) => host.clock().sleep(PRE_RESET_PAUSE),
        Err(text) if first => debug!("Pre-reset failed (likely zombie port): {text}"),
        Err(_) => {}
    }
}

/// Ask for the DOF Reality version text and log its last line; errors are ignored
/// (the label and the raw bytes are two debug lines).
fn log_version_text(open: &mut SerialObject, clock: &mut dyn Clock) {
    if let Ok(output) = open.query(b"[v00]", clock) {
        debug!("first_controller - v00 output: ");
        debug!("{}", py::bytes_repr(wire::last_line(&output)));
    }
}

/// Read Kp and Ki of the four channels and rewrite them (then `[sav]`) when they differ from
/// the integer `MOTIONBOX.reaction_speed` plus 20 (read after the Kp reply) and 1: the gain update
/// of a connect.
fn update_gains(
    open: &mut SerialObject,
    settings: &LiveSettings<Smc3Settings>,
    role: ProbeRole,
    clock: &mut dyn Clock,
) -> Result<(), MotionError> {
    let label = |name: &str| match role {
        ProbeRole::First => name.to_string(),
        ProbeRole::Second => format!("secondary {name}"),
    };
    // The first box reads Kp on one frame line; the second box's read spans two, the setting lookup
    // and its integer conversion. The Kp command for the letters `DEFp` has a frame line of its own
    // for each box.
    let (lookup_line, int_line, command_line) = match role {
        ProbeRole::First => (420, 420, 428),
        ProbeRole::Second => (578, 577, 586),
    };
    let reply = open.query(b"[rdD][rdE][rdF][rdp]", clock)?;
    let kp = settings.read(Smc3Settings::kp).map_err(|e| {
        let line =
            if e.exception_line().is_some_and(|text| text.starts_with("KeyError")) { lookup_line } else { int_line };
        e.in_frame(at("connect", line))
    })?;
    if wire::gains_need_update_wide(&reply, &kp, &label("kp")) {
        open.configure(&gain_command(&native::big(kp)).map_err(|e| e.in_frame(at("connect", command_line)))?, clock)?;
        open.configure(b"[sav]", clock)?;
    }
    let reply = open.query(b"[rdG][rdH][rdI][rdi]", clock)?;
    if wire::gains_need_update(&reply, KI_VALUE, &label("ki")) {
        open.configure(&wire::repeated(b"GHIi", KI_VALUE)?, clock)?;
        open.configure(b"[sav]", clock)?;
    }
    Ok(())
}

/// Write Kp to the four channels, then `[sav]` (stream and test frames).
fn gain_command(kp: &LegacyValue) -> Result<Vec<u8>, MotionError> {
    wire::repeated_native(b"DEFp", kp)
}

fn push_gain(open: &mut SerialObject, kp: &LegacyValue, clock: &mut dyn Clock) -> Result<(), MotionError> {
    open.configure(&gain_command(kp)?, clock)?;
    open.configure(b"[sav]", clock)?;
    Ok(())
}

/// The published actuator positions (`front_left` … `m5`), built at
/// `statement`: ten entries, each reading its slot and rescaling it from 0..4095 to 0..1023, truncated
/// (whatever the box's resolution), assigned once all ten exist.
fn commanded_positions(motors: &LegacyValue, statement: PyFrame) -> Result<ActuatorPositions, MotionError> {
    const SCALE: PyFrame = at("_scale_for_resolution", 699);
    let mut values = [0i64; 10];
    for (value, slot) in values.iter_mut().zip([0, 1, 2, 3, 0, 1, 2, 3, 4, 5]) {
        let raw = native::item(motors, slot).map_err(|e| e.in_frame(statement))?;
        let scaled = wire::to_ten_bit_native(&raw).map_err(|e| e.in_frames([statement, SCALE]))?;
        *value = native::as_i64(&scaled).expect("a 10-bit rescale is in 0..=1023");
    }
    let [front_left, front_right, rear_left, rear_right, m0, m1, m2, m3, m4, m5] = values;
    Ok(ActuatorPositions { front_left, front_right, rear_left, rear_right, motors: [m0, m1, m2, m3, m4, m5] })
}

impl MotionDriver for Smc3Driver {
    /// Connects. Objects of an earlier connection are kept.
    fn connect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        // The debug switch as a boolean; every other setting is read where it is used.
        if self.settings.read(Smc3Settings::debug_enabled).map_err(switch_error(at("connect", 209)))? {
            Self::connect_debug(ctx);
            return Ok(());
        }
        // No search for the `None` model; then store `'Not Connected'`.
        if ctx.model.name == "None" {
            ctx.link.status = LinkStatus::NotConnectedAfterWriteError;
            return Ok(());
        }
        if self.open_configured_ports(ctx)? {
            return Ok(());
        }
        self.scan(ctx)
    }

    /// Close the box(es) and wait 1 s: a 6-motor model closes both objects, any
    /// other model slot 0 only; both slots are then forgotten.
    fn disconnect(&mut self, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        let closing: &[usize] = if ctx.model.motors == TWO_BOX_MOTORS { &[0, 1] } else { &[0] };
        if closing.iter().any(|&i| self.controller[i].is_some()) {
            ctx.link.status = LinkStatus::NotConnectedAfterWriteError;
            for &i in closing {
                if let Some(open) = &self.controller[i] {
                    open.get().close();
                }
            }
            self.controller = [None, None];
        }
        ctx.host.clock().sleep(DISCONNECT_PAUSE);
        Ok(())
    }

    /// A stream frame.
    fn send_stream(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.stream(&native::list_of(motors), ctx, FailurePolicy::KeepOpen)
    }

    /// A test frame.
    fn send_test(&mut self, motors: &[i64], ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.stream(&native::list_of(motors), ctx, FailurePolicy::CloseAll)
    }

    /// A stream frame on the caller's list (which is never written).
    fn send_stream_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.stream(motors, ctx, FailurePolicy::KeepOpen)
    }

    /// A test frame on the caller's list.
    fn send_test_native(&mut self, motors: &mut LegacyValue, ctx: &mut MotionContext<'_>) -> Result<(), MotionError> {
        self.stream(motors, ctx, FailurePolicy::CloseAll)
    }

    /// The debug output of a frame: the UDP mirror.
    fn debug_native(&mut self, motors: &LegacyValue, host: &mut dyn MotionHost) -> Result<(), MotionError> {
        Smc3Driver::debug_native(self, motors, host)
    }

    fn actuators_position(&self) -> Option<ActuatorPositions> {
        self.actuators_position
    }

    /// What the last accepted box reported.
    fn identity(&self) -> Option<ControllerIdentity> {
        Some(self.identity)
    }

    fn describe(&self) -> String {
        let ports: Vec<String> = self.controller.iter().flatten().map(Handle::port).collect();
        if ports.is_empty() {
            "SMC3 (not connected)".into()
        } else {
            format!("SMC3 on {}", ports.join(", "))
        }
    }
}

/// What [`identify_port`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PortIdentity {
    pub version: VersionReply,
    pub resolution: Resolution,
    pub model_id: Option<i64>,
    pub box_slot: Option<BoxSlot>,
}

/// Bring-up helper: run the probe, `[vmd]` and `[v6d]` on one port and close it again,
/// opening it with the default settings: no pre-connection reset, exclusive open.
pub fn identify_port(path: &str, host: &mut dyn MotionHost) -> Result<PortIdentity, MotionError> {
    identify_port_with(path, &Smc3Settings::default(), host)
}

/// [`identify_port`] with the pre-connection reset and the open mode of `settings`.
pub fn identify_port_with(
    path: &str,
    settings: &Smc3Settings,
    host: &mut dyn MotionHost,
) -> Result<PortIdentity, MotionError> {
    if settings.pre_connection_reset {
        pre_reset(path, ProbeRole::First, &LiveSettings::new(settings.clone()), host);
    }
    let options = OpenOptions { baud_rate: BAUD_RATE, write_timeout: Some(WRITE_TIMEOUT) };
    let handle = Handle::new(path, open_port(host, path, &options, settings.exclusive_open)?);
    let mut open = handle.get();
    let frame = read_version_reply(open.link()?, host.clock(), ProbeRole::First)?
        .ok_or_else(|| MotionError::NotFound(path.to_string()))?;
    let model_output = open.query(b"[vmd]", host.clock())?;
    let box_output = open.query(b"[v6d]", host.clock())?;
    Ok(PortIdentity {
        version: wire::decode_version(&frame),
        resolution: wire::reply_resolution(&frame),
        model_id: wire::model_reply(&model_output),
        box_slot: wire::box_reply(&box_output),
    })
}

/// Position feedback (opt-in; the driver itself never reads positions back). The open SMC3
/// firmware answers `[rdA]` with `[A <feedback/4> <target/4>]`.
pub mod diagnostics {
    use super::{BoxSlot, SerialObject, Smc3Driver, COMMAND_PAUSE};
    use crate::motion::clock::Clock;
    use crate::motion::MotionError;

    /// Positions are reported on this scale.
    pub const FULL_SCALE: u32 = 4095;
    const MOTORS_PER_BOX: usize = 3;
    const INBOX_KEEP: usize = 64;

    fn to_full_scale(quarter: u8) -> u32 {
        u32::from(quarter) * 4 * FULL_SCALE / 1023
    }

    /// Latest `(feedback, target)` for motor `letter` in `data`, on the 0..=4095 scale.
    pub fn latest_feedback(data: &[u8], letter: u8) -> Option<(u32, u32)> {
        data.windows(5)
            .rev()
            .find(|w| w[0] == b'[' && w[1] == letter && w[4] == b']')
            .map(|w| (to_full_scale(w[2]), to_full_scale(w[3])))
    }

    fn feedback_request() -> Vec<u8> {
        b"ABC".iter().flat_map(|&l| [b'[', b'r', b'd', l, b']']).collect()
    }

    /// Actuator index of motor `motor` (0..3) of the box in `slot`; box 2 starts at actuator 3.
    fn actuator(slot: BoxSlot, motor: usize) -> usize {
        slot.index() * MOTORS_PER_BOX + motor
    }

    impl Smc3Driver {
        /// Ask every open box for the feedback of its motors without waiting.
        pub fn request_feedback(&mut self) -> Result<(), MotionError> {
            for open in self.controller.iter().flatten() {
                open.get().write(&feedback_request())?;
            }
            Ok(())
        }

        /// Latest `(feedback, target)` per actuator received so far.
        pub fn collect_feedback(&mut self, actuators: usize) -> Result<Vec<Option<(u32, u32)>>, MotionError> {
            let mut out = vec![None; actuators];
            for slot in BoxSlot::BOTH {
                let Some(handle) = &self.controller[slot.index()] else {
                    continue;
                };
                let mut open = handle.get();
                let fresh = open.link()?.read_available()?;
                open.inbox.extend_from_slice(&fresh);
                for (motor, &letter) in b"ABC".iter().enumerate() {
                    if let Some(cell) = out.get_mut(actuator(slot, motor)) {
                        *cell = latest_feedback(&open.inbox, letter);
                    }
                }
                let keep_from = open.inbox.len().saturating_sub(INBOX_KEEP);
                open.inbox.drain(..keep_from);
            }
            Ok(out)
        }

        /// Measured positions of all actuators, when every one of them answered.
        pub fn read_positions(
            &mut self,
            actuators: usize,
            clock: &mut dyn Clock,
        ) -> Result<Option<Vec<u32>>, MotionError> {
            let mut out = vec![None; actuators];
            for slot in BoxSlot::BOTH {
                let Some(handle) = &self.controller[slot.index()] else {
                    continue;
                };
                let reply = query_fresh(&mut handle.get(), clock)?;
                for (motor, &letter) in b"ABC".iter().enumerate() {
                    if let Some(cell) = out.get_mut(actuator(slot, motor)) {
                        *cell = latest_feedback(&reply, letter).map(|(position, _)| position);
                    }
                }
            }
            Ok(out.into_iter().collect())
        }
    }

    fn query_fresh(open: &mut SerialObject, clock: &mut dyn Clock) -> Result<Vec<u8>, MotionError> {
        open.link()?.clear_input()?;
        open.write(&feedback_request())?;
        clock.sleep(COMMAND_PAUSE);
        Ok(open.link()?.read_available()?)
    }
}

#[cfg(test)]
mod tests {
    use super::wire::*;
    use super::*;
    use crate::motion::testing::{Device, FakeHost};
    use crate::motion::{motion_model, LinkState};
    use crate::serial::{ClosedLink, Link, LinkError, OpenOptions};

    const VER_12_BIT: &[u8] = b"[v\x00\xd2]4]";
    const VER_10_BIT: &[u8] = b"[v\x00\xd2]";

    fn gains(value: u8) -> Vec<u8> {
        repeated(b"DEFp", i64::from(value)).unwrap()
    }

    fn ki(value: u8) -> Vec<u8> {
        repeated(b"GHIi", i64::from(value)).unwrap()
    }

    /// A DOF Reality box: version reply, model code, box number, gains already at kp 120 / ki 1.
    fn dof_box(version: &[u8], model: u8, number: Option<u8>) -> Device {
        let mut replies = vec![
            (b"[ver]".to_vec(), version.to_vec()),
            (b"[vmd]".to_vec(), vec![b'[', b'v', b'm', model, b']']),
            (b"[v00]".to_vec(), b"name_hex:UP6\nID_MODEL=6 v6d=1".to_vec()),
            (b"[rdD][rdE][rdF][rdp]".to_vec(), gains(120)),
            (b"[rdG][rdH][rdI][rdi]".to_vec(), ki(1)),
        ];
        if let Some(n) = number {
            replies.push((b"[v6d]".to_vec(), vec![b'[', b'v', 0, b'0' + n, b']']));
        }
        Device { replies, ..Device::default() }
    }

    fn driver(settings: Smc3Settings) -> Smc3Driver {
        Smc3Driver::new(LiveSettings::new(settings))
    }

    fn connect(driver: &mut Smc3Driver, host: &mut FakeHost, model: &str) -> LinkState {
        let mut link = LinkState::default();
        let spec = motion_model(model, false).unwrap();
        driver.connect(&mut MotionContext { model: spec, link: &mut link, host }).unwrap();
        link
    }

    #[test]
    fn enumeration_failure_stops_before_open_and_keeps_the_existing_link_state() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB0"]);
        host.port_list_error = Some("sysfs field disappeared".into());
        let mut driver = driver(Smc3Settings::default());
        let mut link = LinkState::default();
        let before = link.status;
        let model = motion_model("P3", false).unwrap();
        let error = driver.connect(&mut MotionContext { model, link: &mut link, host: &mut host }).unwrap_err();
        assert_eq!(error.to_string(), "sysfs field disappeared");
        assert_eq!(link.status, before);
        assert!(link.ports.is_empty());
        assert!(host.log("/dev/ttyUSB0").opens.is_empty());
        assert_eq!(host.clock.now(), Duration::ZERO);
    }

    fn stream(
        driver: &mut Smc3Driver,
        host: &mut FakeHost,
        link: &mut LinkState,
        motors: &[i64],
    ) -> Result<(), MotionError> {
        let spec = motion_model("P3", false).unwrap();
        driver.send_stream(motors, &mut MotionContext { model: spec, link, host })
    }

    fn writes(host: &mut FakeHost, path: &str) -> Vec<Vec<u8>> {
        host.log(path).written()
    }

    /// The Kp command for the letters `DEFp` with 120: a byte example.
    #[test]
    fn gen_smc3_command_bytes() {
        assert_eq!(
            repeated(b"DEFp", 120).unwrap(),
            [
                0x5B, 0x44, 0, 0x78, 0x5D, 0x5B, 0x45, 0, 0x78, 0x5D, 0x5B, 0x46, 0, 0x78, 0x5D, 0x5B, 0x70, 0, 0x78,
                0x5D
            ]
        );
        assert_eq!(repeated(b"GHIi", 1).unwrap()[..5], [0x5B, 0x47, 0, 1, 0x5D]);
        assert_eq!(repeated(b"D", 65536).unwrap_err().python_str(), "int too big to convert");
        assert_eq!(repeated(b"D", -1).unwrap_err().python_str(), "can't convert negative int to unsigned");
    }

    /// The four-letter position command at resolution 10, all slots at the centre 2048.
    #[test]
    fn gen_abcy_command_ten_bit_centre() {
        assert_eq!(
            positions(b"ABCY", &[2048; 4], Resolution::TEN_BIT).unwrap(),
            [
                0x5B, 0x41, 0x01, 0xFF, 0x5D, 0x5B, 0x42, 0x01, 0xFF, 0x5D, 0x5B, 0x43, 0x01, 0xFF, 0x5D, 0x5B, 0x59,
                0x01, 0xFF, 0x5D
            ]
        );
    }

    /// The three-letter position command: a resolution other than 10 sends values unchanged, but they must fit 16 bits.
    #[test]
    fn gen_abc_command_unscaled_for_other_resolutions() {
        assert_eq!(positions(b"A", &[3891], Resolution::TWELVE_BIT).unwrap(), [b'[', b'A', 0x0F, 0x33, b']']);
        assert_eq!(positions(b"A", &[3891], Resolution(16)).unwrap(), [b'[', b'A', 0x0F, 0x33, b']']);
        assert!(positions(b"A", &[70000], Resolution::TWELVE_BIT).is_err());
        assert_eq!(positions(b"A", &[70000], Resolution::TEN_BIT).unwrap(), [b'[', b'A', 0x03, 0xFF, b']']);
    }

    /// The rescale from 0..4095 to 0..1023: truncation and clamping.
    #[test]
    fn ten_bit_rescale_truncates_and_clamps() {
        assert_eq!(to_ten_bit(204), 50);
        assert_eq!(to_ten_bit(3891), 972);
        assert_eq!(to_ten_bit(2048), 511);
        assert_eq!(to_ten_bit(4095), 1023);
        assert_eq!(to_ten_bit(9999), 1023);
        assert_eq!(to_ten_bit(-5), 0);
        assert_eq!(to_ten_bit(4), 0);
        assert_eq!(to_ten_bit(5), 1);
    }

    /// Finding a response: first matching frame, `4]` suffix only for `v`, frames of other commands skipped.
    #[test]
    fn find_response_frames() {
        let data = b"junk[D\x00\x01][v\x00\xd2]4]tail";
        assert_eq!(find_reply(data, Some(b'v')).unwrap(), VER_12_BIT);
        assert_eq!(find_reply(data, None).unwrap(), b"[D\x00\x01]");
        assert_eq!(find_reply(b"[A\x00\x01]4]", Some(b'A')).unwrap(), b"[A\x00\x01]");
        assert!(find_reply(b"[D\x00\x01]", Some(b'v')).is_none());
        assert!(find_reply(b"[v\x00", Some(b'v')).is_none());
        assert!(find_reply(b"", Some(b'v')).is_none());
        // A value byte equal to `]` ends the frame early.
        assert_eq!(find_reply(b"[v\x01]]", Some(b'v')).unwrap(), b"[v\x01]");
    }

    fn ver(n: u16) -> Vec<u8> {
        let [hi, lo] = n.to_be_bytes();
        vec![b'[', b'v', hi, lo, b']']
    }

    /// Decoding a version: 3 digits → (-1, n); 4 digits → (first digit, last three); otherwise (-1, -1).
    #[test]
    fn decode_ver_digit_rules() {
        let d = |frame: &[u8]| {
            let r = decode_version(frame);
            (r.leading_digit, r.version)
        };
        assert_eq!(d(&ver(210)), (-1, 210));
        assert_eq!(d(&ver(101)), (-1, 101));
        assert_eq!(d(&ver(1234)), (1, 234));
        assert_eq!(d(&ver(3050)), (3, 50));
        assert_eq!(d(&ver(42)), (-1, -1));
        assert_eq!(d(&ver(12345)), (-1, -1));
        assert_eq!(d(&ver(0)), (-1, -1));
        assert_eq!(d(VER_12_BIT), (-1, 210));
        assert_eq!(d(b"[v\x00\x00\x00\xd2]"), (-1, 210));
        assert_eq!(d(b"[v\x01\x00\x00]"), (-1, -1));
        assert_eq!(d(b"[v\x01]"), (-1, -1));
    }

    /// The controller resolution: 12 only with the `]4]` marker.
    #[test]
    fn controller_resolution_marker() {
        assert_eq!(reply_resolution(VER_12_BIT), Resolution::TWELVE_BIT);
        assert_eq!(reply_resolution(VER_10_BIT), Resolution::TEN_BIT);
    }

    /// The parsing of the `[vmd]` and `[v6d]` replies.
    #[test]
    fn identity_replies() {
        assert_eq!(model_reply(b"boot\n[vm\x06]"), Some(6));
        assert_eq!(model_reply(b"[vm"), Some(i64::from(b'v')));
        assert_eq!(model_reply(b"[vm\x06]\n"), None);
        assert_eq!(model_reply(b"nothing"), None);
        assert_eq!(box_reply(b"[v\x002]"), Some(BoxSlot::Right));
        assert_eq!(box_reply(b"x\n[v\x001]"), Some(BoxSlot::Left));
        assert_eq!(box_reply(b"[v\x003]"), None);
        assert_eq!(box_reply(b"[v\x001]junk"), None);
    }

    /// The update test: positions 3, 8, 13; short replies ask for no update; frame 4 is not its concern.
    #[test]
    fn should_update_smc3_params_rules() {
        let reply = gains(120);
        assert!(!first_three_differ(&reply, 120, "kp"));
        assert!(first_three_differ(&reply, 110, "kp"));
        assert!(!first_three_differ(&reply[..13], 110, "kp"));
        let mut fourth_differs = reply.clone();
        fourth_differs[18] = 7;
        assert!(!first_three_differ(&fourth_differs, 120, "kp"));
        let mut second_differs = reply;
        second_differs[8] = 7;
        assert!(first_three_differ(&second_differs, 120, "kp"));
    }

    /// The gain update of a connect: the update test, or a reply longer than 18 bytes whose byte 18 differs.
    #[test]
    fn connect_l424_l447_gain_decision_also_checks_the_fourth_frame() {
        let reply = gains(120);
        assert!(!gains_need_update(&reply, 120, "kp"));
        let mut fourth_differs = reply.clone();
        fourth_differs[18] = 0x64;
        assert!(gains_need_update(&fourth_differs, 120, "kp"));
        assert!(!gains_need_update(&fourth_differs[..18], 120, "kp"), "18 bytes: no byte 18 to compare");
        let mut first_differs = reply.clone();
        first_differs[3] = 7;
        assert!(gains_need_update(&first_differs, 120, "kp"));
        assert!(!gains_need_update(&reply[..13], 110, "kp"), "too short for both checks");
        assert!(gains_need_update(&gains(120), 300, "kp"), "kp above 255 never matches");
    }

    /// Connect: the left controller resolution as an integer, 10 when it is not one.
    #[test]
    fn configured_resolution_parses_like_python_int() {
        let configured_resolution = |text: &str| native_configured_resolution(&Ok(LegacyValue::from(text)));
        assert_eq!(configured_resolution("12"), Resolution(12));
        assert_eq!(configured_resolution(" 12\n"), Resolution(12));
        assert_eq!(configured_resolution("1_2"), Resolution(12));
        assert_eq!(configured_resolution("+10"), Resolution(10));
        assert_eq!(configured_resolution("1__2"), Resolution(10));
        assert_eq!(configured_resolution("12.0"), Resolution(10));
        assert_eq!(configured_resolution(""), Resolution(10));
        assert_eq!(configured_resolution("-3"), Resolution(-3));
        assert_eq!(configured_resolution("１２"), Resolution(12));
        assert_eq!(configured_resolution("١_٠"), Resolution(10));
        assert_eq!(configured_resolution("\u{1c}12"), Resolution(10));
        assert_eq!(configured_resolution("-9223372036854775808"), Resolution(i64::MIN));
        assert!(!configured_resolution("99999999999999999999999999999999999999").scales_to_ten_bit());
        assert_eq!(configured_resolution(&format!("{}12", "0".repeat(4298))), Resolution(12));
        assert_eq!(configured_resolution(&format!("{}12", "0".repeat(4299))), Resolution(10));
    }

    /// Connect with the default settings: no pre-reset, one open with a write timeout of 3 s, first
    /// [ver] 1.0 s after opening.
    #[test]
    fn connect_l297_windows_build_probes_without_pre_reset() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_12_BIT, 1, None));
        let mut d = driver(Smc3Settings::default());
        assert!(connect(&mut d, &mut host, "P3").is_connected());
        let log = host.log("/dev/ttyUSB1");
        assert_eq!(log.opens.len(), 1);
        assert_eq!(log.opens[0].write_timeout, Some(Duration::from_secs(3)));
        assert_eq!(log.clears, 1, "only the flush before the [ver] send");
        assert_eq!(log.writes[0], (Duration::from_millis(2000), b"[ver]".to_vec()));
    }

    /// Connect with the pre-reset enabled: pre-reset (open/flush/close + 1.5 s), real open with a write timeout
    /// of 3 s, first [ver] 2.0 s after opening.
    #[test]
    fn probe_timing_after_linux_pre_reset() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_12_BIT, 1, None));
        let mut d = driver(Smc3Settings { pre_connection_reset: true, ..Smc3Settings::default() });
        let link = connect(&mut d, &mut host, "P3");
        assert!(link.is_connected());
        let log = host.log("/dev/ttyUSB1");
        assert_eq!(log.opens.len(), 2);
        assert_eq!(log.opens[0].write_timeout, None);
        assert_eq!(log.opens[1].write_timeout, Some(Duration::from_secs(3)));
        assert_eq!(log.opens[1].baud_rate, 500_000);
        assert_eq!(log.writes[0], (Duration::from_millis(3500), b"[ver]".to_vec()));
    }

    /// connect: [ver] first sent after 2 s, then resent every second, with an input flush before each send.
    #[test]
    fn probe_resends_ver_every_second() {
        let device = Device { ver_reply_from: 2, ..dof_box(VER_10_BIT, 1, None) };
        let mut host = FakeHost::with_ports(&["/dev/ttyACM0"]).device("/dev/ttyACM0", device);
        let mut d = driver(Smc3Settings::default());
        assert!(connect(&mut d, &mut host, "P3").is_connected());
        let log = host.log("/dev/ttyACM0");
        let ver_times: Vec<Duration> = log.writes.iter().filter(|(_, w)| w == b"[ver]").map(|(t, _)| *t).collect();
        let ms = |v| Duration::from_millis(v);
        assert_eq!(ver_times, [ms(2000), ms(3000), ms(4000)]);
        assert_eq!(log.clears, 3, "one flush per send");
    }

    /// connect: a silent port is given up after 10 s (eight [ver] sends) and the scan goes on.
    #[test]
    fn silent_port_times_out_after_ten_seconds() {
        let mut host =
            FakeHost::with_ports(&["/dev/ttyS0", "/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_12_BIT, 1, None));
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P3");
        assert_eq!(link.ports.get(&0).map(String::as_str), Some("/dev/ttyUSB1"));
        let silent: Vec<Duration> = host.log("/dev/ttyS0").writes.iter().map(|(t, _)| *t).collect();
        let ms = |v| Duration::from_millis(v);
        assert_eq!(silent, [ms(2000), ms(3000), ms(4000), ms(5000), ms(6000), ms(7000), ms(8000), ms(9000)]);
        let second_open = host.log("/dev/ttyUSB1").writes[0].0;
        assert_eq!(second_open, ms(10_000 + 2000));
    }

    /// connect: more than 128 bytes without '[' abandons the port before any [ver].
    #[test]
    fn unframed_noise_abandons_port() {
        let noise = Device { on_open: vec![b'x'; 129], ..Device::default() };
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB0"]).device("/dev/ttyUSB0", noise);
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P3");
        assert!(!link.is_connected());
        assert!(host.log("/dev/ttyUSB0").writes.is_empty());
        assert_eq!(host.now(), Duration::ZERO);
        let exactly = Device { on_open: vec![b'x'; 128], ..Device::default() };
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB0"]).device("/dev/ttyUSB0", exactly);
        connect(&mut d, &mut host, "P3");
        assert_eq!(host.log("/dev/ttyUSB0").writes.len(), 8, "128 bytes are not enough to give up");
    }

    /// Connect: a single 12-bit DOF box with matching gains.
    #[test]
    fn single_box_handshake_sequence() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_12_BIT, 1, None));
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P3");
        assert_eq!(link.status, LinkStatus::Connected);
        assert_eq!(link.ports.len(), 1);
        assert_eq!(
            writes(&mut host, "/dev/ttyUSB1"),
            [&b"[ver]"[..], b"[vmd]", b"[v00]", b"[rdD][rdE][rdF][rdp]", b"[rdG][rdH][rdI][rdi]"]
        );
        assert_eq!(
            d.identity().unwrap(),
            ControllerIdentity { version: Some(210), model_id: Some(1), reported_motors: Some(-1) }
        );
        assert_eq!(d.frame_resolution()[0], Resolution::TWELVE_BIT);
    }

    /// connect: differing Kp/Ki are rewritten and saved; reaction speed 90 → kp 110.
    #[test]
    fn gains_rewritten_when_they_differ() {
        let mut device = dof_box(VER_10_BIT, 1, None);
        device.replies[3].1 = gains(100);
        device.replies[4].1 = ki(5);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", device);
        let mut d = driver(Smc3Settings { reaction_speed: 90, ..Smc3Settings::default() });
        connect(&mut d, &mut host, "P3");
        let w = writes(&mut host, "/dev/ttyUSB1");
        assert_eq!(w[4], gains(110));
        assert_eq!(w[5], b"[sav]");
        assert_eq!(w[6], b"[rdG][rdH][rdI][rdi]");
        assert_eq!(w[7], ki(1));
        assert_eq!(w[8], b"[sav]");
    }

    /// Connect: D/E/F (G/H/I) match but the 4th frame p (i) differs → rewrite and [sav].
    #[test]
    fn connect_l424_l447_fourth_frame_mismatch_rewrites_gains() {
        let mut device = dof_box(VER_10_BIT, 1, None);
        device.replies[3].1[18] = 0x64;
        device.replies[4].1[18] = 0x05;
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", device);
        let mut d = driver(Smc3Settings::default());
        connect(&mut d, &mut host, "P3");
        let w = writes(&mut host, "/dev/ttyUSB1");
        assert_eq!(w[3], b"[rdD][rdE][rdF][rdp]");
        assert_eq!(w[4], gains(120));
        assert_eq!(w[5], b"[sav]");
        assert_eq!(w[6], b"[rdG][rdH][rdI][rdi]");
        assert_eq!(w[7], ki(1));
        assert_eq!(w[8], b"[sav]");
    }

    /// Connect: the same 4th-frame comparison for the second box ('secondary kp' / 'secondary ki').
    #[test]
    fn connect_l582_l606_fourth_frame_mismatch_rewrites_second_box_gains() {
        let mut second = dof_box(VER_10_BIT, 6, Some(2));
        second.replies[3].1[18] = 0x64;
        second.replies[4].1[18] = 0x05;
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
            .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(1)))
            .device("/dev/ttyUSB2", second);
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P6");
        assert!(link.is_connected());
        assert_eq!(
            writes(&mut host, "/dev/ttyUSB2"),
            [
                &b"[ver]"[..],
                b"[v6d]",
                b"[rdD][rdE][rdF][rdp]",
                &gains(120),
                b"[sav]",
                b"[rdG][rdH][rdI][rdi]",
                &ki(1),
                b"[sav]"
            ]
        );
    }

    /// connect: with update_kp off the gains are neither read nor written.
    #[test]
    fn gains_untouched_without_update_kp() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_10_BIT, 1, None));
        let mut d = driver(Smc3Settings { update_kp: false, ..Smc3Settings::default() });
        connect(&mut d, &mut host, "P3");
        assert_eq!(writes(&mut host, "/dev/ttyUSB1"), [&b"[ver]"[..], b"[vmd]", b"[v00]"]);
    }

    /// Connect: a box without a [vmd] answer is placed as box 1 right away.
    #[test]
    fn generic_smc3_without_model_reply_connects_as_one_box() {
        let device = Device::replying(&[(b"[ver]", VER_10_BIT)]);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", device);
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P6");
        assert!(link.is_connected());
        assert_eq!(link.ports.len(), 1);
        assert_eq!(writes(&mut host, "/dev/ttyUSB1"), [&b"[ver]"[..], b"[vmd]"]);
        assert_eq!(d.identity().unwrap().model_id, Some(6));
        assert_eq!(d.identity().unwrap().reported_motors, Some(6));
    }

    /// Connect: version -1 and versions below 100 are rejected and the port closed.
    #[test]
    fn rejected_versions_continue_the_scan() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB0", "/dev/ttyUSB1", "/dev/ttyUSB2"])
            .device("/dev/ttyUSB0", Device::replying(&[(b"[ver]", &ver(42))]))
            .device("/dev/ttyUSB1", Device::replying(&[(b"[ver]", &ver(1050))]))
            .device("/dev/ttyUSB2", dof_box(&ver(1234), 1, None));
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P3");
        assert_eq!(link.ports.get(&0).map(String::as_str), Some("/dev/ttyUSB2"));
        assert_eq!(d.identity().unwrap().version, Some(234));
        assert_eq!(d.identity().unwrap().reported_motors, Some(1));
        assert_eq!(writes(&mut host, "/dev/ttyUSB0"), [&b"[ver]"[..]]);
    }

    /// connect: excluded, disallowed and claimed ports are never opened.
    #[test]
    fn scan_skips_filtered_ports() {
        let mut host = FakeHost::with_ports(&["/dev/ttyAMA0", "/dev/ttyUSB0", "/dev/ttyUSB1"]);
        host.claimed.motion_compensation = Some("/dev/ttyUSB0".into());
        host.allowed = "/dev/ttyAMA0,/dev/ttyUSB0".into();
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P3");
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        for p in ["/dev/ttyAMA0", "/dev/ttyUSB0", "/dev/ttyUSB1"] {
            assert!(host.log(p).opens.is_empty(), "{p} opened");
        }
    }

    /// connect: the box number decides the slot, not the port order; box 2 needs no [vmd].
    #[test]
    fn two_box_rig_assigns_slots_by_box_number() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
            .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(2)))
            .device("/dev/ttyUSB2", dof_box(VER_10_BIT, 6, Some(1)));
        let mut d = driver(Smc3Settings::default());
        let mut link = connect(&mut d, &mut host, "P6");
        assert!(link.is_connected());
        assert_eq!(link.ports[&0], "/dev/ttyUSB2");
        assert_eq!(link.ports[&1], "/dev/ttyUSB1");
        assert_eq!(
            writes(&mut host, "/dev/ttyUSB2"),
            [&b"[ver]"[..], b"[v6d]", b"[rdD][rdE][rdF][rdp]", b"[rdG][rdH][rdI][rdi]"]
        );
        assert_eq!(writes(&mut host, "/dev/ttyUSB1").last().unwrap(), b"[v6d]");

        stream(&mut d, &mut host, &mut link, &[100, 200, 300, 400, 500, 600, 0, 0, 0, 0, 0, 0]).unwrap();
        let left = writes(&mut host, "/dev/ttyUSB2").pop().unwrap();
        let right = writes(&mut host, "/dev/ttyUSB1").pop().unwrap();
        let expected_left = positions(b"ABCY", &[100, 200, 300, 400], Resolution::TEN_BIT).unwrap();
        assert_eq!(left, expected_left);
        assert_eq!(left.len(), 20);
        assert_eq!(right, positions(b"ABC", &[400, 500, 600], Resolution::TEN_BIT).unwrap());
    }

    /// Connect, then a stream frame: frame scaling follows the probe order, so when box 2
    /// is found first its resolution scales the box-1 frame.
    #[test]
    fn frame_resolution_follows_probe_order() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
            .device("/dev/ttyUSB1", dof_box(VER_12_BIT, 6, Some(2)))
            .device("/dev/ttyUSB2", dof_box(VER_10_BIT, 6, Some(1)));
        let mut d = driver(Smc3Settings::default());
        let mut link = connect(&mut d, &mut host, "P6");
        assert_eq!(d.frame_resolution(), [Resolution::TWELVE_BIT, Resolution::TEN_BIT]);
        stream(&mut d, &mut host, &mut link, &[3000; 6]).unwrap();
        let left = writes(&mut host, "/dev/ttyUSB2").pop().unwrap();
        assert_eq!(left[..5], [b'[', b'A', 0x0B, 0xB8, b']']);
    }

    /// connect: without a second box the scan goes on and ends `Not Connected`; the first box keeps
    /// its slot and port entry.
    #[test]
    fn two_box_rig_with_one_box_ends_not_connected() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
            .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(1)));
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P6");
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(link.ports[&0], "/dev/ttyUSB1");
        assert_eq!(host.log("/dev/ttyUSB2").opens.len(), 2, "probed as second box and as first box");
    }

    /// connect: a first box that answers neither 1 nor 2 is dropped and the outer scan goes on (no
    /// second-box search from it). USB2 then becomes the first box (slot 1); its search re-probes USB1, which
    /// again names no box, so the scan ends `Not Connected`.
    #[test]
    fn connect_l472_l478_unnumbered_first_box_continues_the_outer_scan() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
            .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(7)))
            .device("/dev/ttyUSB2", dof_box(VER_10_BIT, 6, Some(2)));
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P6");
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(link.ports.len(), 1);
        assert_eq!(link.ports[&1], "/dev/ttyUSB2");
        let usb2 = writes(&mut host, "/dev/ttyUSB2");
        assert_eq!(usb2[0], b"[ver]");
        assert_eq!(usb2[1], b"[vmd]", "USB2 was probed as a first box, not as a second box");
        assert_eq!(host.log("/dev/ttyUSB2").opens.len(), 1, "one probe open");
    }

    /// connect: a second box with an unknown number is not placed and the inner search goes on;
    /// only a placed box ends the scan as `Connected`.
    #[test]
    fn connect_l620_l632_unnumbered_second_box_continues_the_inner_search() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2", "/dev/ttyUSB3"])
            .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(1)))
            .device("/dev/ttyUSB2", dof_box(VER_10_BIT, 6, Some(7)))
            .device("/dev/ttyUSB3", dof_box(VER_10_BIT, 6, Some(2)));
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P6");
        assert!(link.is_connected());
        assert_eq!(link.ports[&0], "/dev/ttyUSB1");
        assert_eq!(link.ports[&1], "/dev/ttyUSB3");
        assert_eq!(
            writes(&mut host, "/dev/ttyUSB2"),
            [&b"[ver]"[..], b"[v6d]", b"[rdD][rdE][rdF][rdp]", b"[rdG][rdH][rdI][rdi]"]
        );
    }

    /// connect: with only an unnumbered second box the scan does not report `Connected`.
    #[test]
    fn connect_l620_l632_unnumbered_second_box_alone_is_not_connected() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
            .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(1)))
            .device("/dev/ttyUSB2", dof_box(VER_10_BIT, 6, Some(7)));
        let mut d = driver(Smc3Settings::default());
        let link = connect(&mut d, &mut host, "P6");
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(link.ports.get(&1), None);
    }

    /// connect: configured ports are opened without probing or write timeout.
    #[test]
    fn configured_ports_skip_the_scan() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"]);
        let mut d = driver(Smc3Settings {
            left_controller_com_port: "/dev/ttyUSB2".into(),
            left_controller_resolution: "12".into(),
            right_controller_resolution: "x".into(),
            ..Smc3Settings::default()
        });
        let link = connect(&mut d, &mut host, "P3");
        assert!(link.is_connected());
        assert_eq!(link.ports.len(), 1);
        assert_eq!(link.ports[&0], "/dev/ttyUSB2");
        let log = host.log("/dev/ttyUSB2");
        assert_eq!(log.opens.len(), 1);
        assert_eq!(log.opens[0].write_timeout, None);
        assert!(log.writes.is_empty());
        drop(log);
        assert!(host.log("/dev/ttyUSB1").opens.is_empty());
        assert_eq!(d.frame_resolution(), [Resolution(12), Resolution::TEN_BIT]);
    }

    /// connect: a configured port that cannot be opened ends the attempt.
    #[test]
    fn configured_port_open_failure_ends_connect() {
        let busy = Device { fail_open: true, ..Device::default() };
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB3", busy);
        let mut d = driver(Smc3Settings {
            left_controller_com_port: "/dev/ttyUSB3".into(),
            right_controller_com_port: "/dev/ttyUSB4".into(),
            ..Smc3Settings::default()
        });
        let link = connect(&mut d, &mut host, "P6");
        assert_eq!(link.status, LinkStatus::NotConnected);
        assert!(link.ports.is_empty());
        assert!(host.log("/dev/ttyUSB4").opens.is_empty());
        assert!(host.log("/dev/ttyUSB1").opens.is_empty());
    }

    /// Connect and stream frames in debug mode: fake ports, UDP mirror only.
    #[test]
    fn debug_mode_fakes_connection_and_mirrors_frames() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]);
        let mut d = driver(Smc3Settings { enable_debug: true, ..Smc3Settings::default() });
        let mut link = connect(&mut d, &mut host, "P4");
        assert!(link.is_connected());
        assert_eq!(link.ports.values().collect::<Vec<_>>(), ["COMX", "COMY"]);
        assert_eq!(connect(&mut d, &mut host, "P3").ports.len(), 1);
        stream(&mut d, &mut host, &mut link, &[2048; 12]).unwrap();
        assert!(host.log("/dev/ttyUSB1").opens.is_empty());
        let targets: Vec<String> = host.datagrams.iter().map(|(t, _)| t.to_string()).collect();
        assert_eq!(targets, ["127.0.0.1:20017", "127.0.0.1:20018"]);
        assert_eq!(host.datagram_sockets, MIRROR_SOCKETS, "first_sock, second_sock");
        assert_eq!(host.datagrams[0].1, positions(b"ABCY", &[2048; 4], Resolution::TEN_BIT).unwrap());
        assert_eq!(host.datagrams[1].1.len(), 15);
    }

    #[test]
    fn native_startup_keeps_debug_port_capture_and_later_invalid_target_catch() {
        let settings = LiveSettings::new(Smc3Settings {
            native_startup: Some(Ok(NativeSmc3Startup {
                debug_ports: [None, Some(30_018)],
                previous_kp: BigInt::from(75),
            })),
            ..Smc3Settings::default()
        });
        let driver = Smc3Driver::new(settings.clone());
        assert_eq!(driver.previous_kp, BigInt::from(75));
        settings.update(|settings| {
            settings.native_startup = Some(Ok(NativeSmc3Startup {
                debug_ports: [Some(20_017), Some(20_018)],
                previous_kp: BigInt::from(120),
            }));
        });
        let mut host = FakeHost::default();
        driver.debug(&[2048; 6], &mut host).unwrap();
        assert_eq!(host.datagrams.len(), 1, "out-of-range first target is caught; second target still sends");
        assert_eq!(host.datagrams[0].0.to_string(), "127.0.0.1:30018", "startup target is captured once");
        assert_eq!(host.datagram_sockets, [MIRROR_SOCKETS[1]]);
    }

    fn connected_p3(settings: Smc3Settings) -> (Smc3Driver, FakeHost, LinkState) {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_10_BIT, 1, None));
        let mut d = driver(settings);
        let link = connect(&mut d, &mut host, "P3");
        host.log("/dev/ttyUSB1").writes.clear();
        (d, host, link)
    }

    /// send_stream: one 20-byte ABCY frame (Y = slot 3), actuators_position in 10 bit.
    #[test]
    fn send_stream_frame_and_actuator_positions() {
        let (mut d, mut host, mut link) = connected_p3(Smc3Settings::default());
        stream(&mut d, &mut host, &mut link, &[204, 2048, 3891, 2048, 7, 8]).unwrap();
        assert_eq!(
            writes(&mut host, "/dev/ttyUSB1"),
            [positions(b"ABCY", &[204, 2048, 3891, 2048], Resolution::TEN_BIT).unwrap()]
        );
        let p = d.actuators_position().unwrap();
        assert_eq!((p.front_left, p.front_right, p.rear_left, p.rear_right), (50, 511, 972, 511));
        assert_eq!(p.motors, [50, 511, 972, 511, 1, 1]);
    }

    /// send_stream: a reaction-speed change writes DEFp and [sav] once.
    #[test]
    fn send_stream_pushes_kp_when_reaction_speed_changes() {
        let settings = LiveSettings::new(Smc3Settings::default());
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_10_BIT, 1, None));
        let mut d = Smc3Driver::new(settings.clone());
        let mut link = connect(&mut d, &mut host, "P3");
        host.log("/dev/ttyUSB1").writes.clear();
        stream(&mut d, &mut host, &mut link, &[2048; 6]).unwrap();
        assert_eq!(writes(&mut host, "/dev/ttyUSB1").len(), 1, "unchanged speed: frame only");
        settings.update(|s| s.reaction_speed = 80);
        let before = host.now();
        stream(&mut d, &mut host, &mut link, &[2048; 6]).unwrap();
        stream(&mut d, &mut host, &mut link, &[2048; 6]).unwrap();
        let w = writes(&mut host, "/dev/ttyUSB1");
        assert_eq!(w.len(), 5);
        assert_eq!(w[2], gains(100));
        assert_eq!(w[3], b"[sav]");
        assert_eq!(host.now() - before, Duration::from_millis(200));
    }

    /// send_stream: with update_kp off nothing is pushed, but the speed is remembered.
    #[test]
    fn send_stream_remembers_speed_without_update_kp() {
        let settings = LiveSettings::new(Smc3Settings { update_kp: false, ..Smc3Settings::default() });
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_10_BIT, 1, None));
        let mut d = Smc3Driver::new(settings.clone());
        let mut link = connect(&mut d, &mut host, "P3");
        settings.update(|s| s.reaction_speed = 80);
        stream(&mut d, &mut host, &mut link, &[2048; 6]).unwrap();
        settings.update(|s| s.update_kp = true);
        stream(&mut d, &mut host, &mut link, &[2048; 6]).unwrap();
        assert!(!writes(&mut host, "/dev/ttyUSB1").contains(&gains(100)));
    }

    #[test]
    fn native_update_kp_error_keeps_written_frame_and_marks_link_failed() {
        let (mut driver, mut host, mut link) = connected_p3(Smc3Settings::default());
        driver.settings.update(|settings| {
            settings.reaction_speed = 80;
            settings.native_switches = Some(NativeSmc3Switches {
                enable_debug: Ok(false),
                update_kp: Err("Exception: Boolean value expected. Got maybe".into()),
            });
        });
        stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
        assert_eq!(writes(&mut host, "/dev/ttyUSB1"), [positions(b"ABCY", &[2048; 4], Resolution::TEN_BIT).unwrap()]);
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert!(driver.is_open(BoxSlot::Left));
        assert!(driver.actuators_position().is_some());
        assert_eq!(driver.previous_kp, BigInt::from(100));
    }

    #[test]
    fn native_reaction_error_precedes_frames_and_preserves_previous_state() {
        let (mut driver, mut host, mut link) = connected_p3(Smc3Settings::default());
        driver.settings.update(|settings| {
            settings.native_reaction_speed =
                Some(Err("ValueError: invalid literal for int() with base 10: '80.5'".into()));
        });
        let error = stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap_err();
        assert_eq!(error.python_str(), "invalid literal for int() with base 10: '80.5'");
        assert!(writes(&mut host, "/dev/ttyUSB1").is_empty());
        assert_eq!(driver.previous_kp, BigInt::from(120));
        assert!(driver.actuators_position().is_none());
        let model = motion_model("P3", false).unwrap();
        assert!(driver.send_test(&[2048; 6], &mut MotionContext { model, link: &mut link, host: &mut host }).is_err());
        assert!(writes(&mut host, "/dev/ttyUSB1").is_empty());
        driver.settings.update(|settings| settings.native_reaction_speed = Some(Ok(LegacyValue::Int(80))));
        stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
        let sent = writes(&mut host, "/dev/ttyUSB1");
        assert_eq!(sent, [positions(b"ABCY", &[2048; 4], Resolution::TEN_BIT).unwrap(), gains(100), b"[sav]".to_vec()]);
        assert_eq!(driver.previous_kp, BigInt::from(100));
        driver.settings.update(|settings| {
            settings.enable_debug = true;
            settings.native_reaction_speed = Some(Err("KeyError: 'reaction_speed'".into()));
        });
        stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
        assert_eq!(host.datagrams.len(), 2, "debug does not reach int(reaction_speed)");
        assert_eq!(driver.previous_kp, BigInt::from(100));
    }

    #[test]
    fn native_reaction_conversion_fails_after_gain_reply_before_gain_writes() {
        struct InvalidatingLink {
            settings: LiveSettings<Smc3Settings>,
            writes: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl Link for InvalidatingLink {
            fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
                self.writes.lock().unwrap().push(bytes.to_vec());
                self.settings.update(|settings| {
                    settings.native_reaction_speed = Some(Err("TypeError: native reaction fixture".into()));
                });
                Ok(())
            }
            fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
                Ok(Vec::new())
            }
            fn clear_input(&mut self) -> Result<(), LinkError> {
                Ok(())
            }
            fn name(&self) -> &str {
                "fake gain reply"
            }
        }
        let settings = LiveSettings::new(Smc3Settings::default());
        let writes = Arc::new(Mutex::new(Vec::new()));
        let link = InvalidatingLink { settings: settings.clone(), writes: writes.clone() };
        let mut open = SerialObject { port: "fake".into(), link: Some(Box::new(link)), inbox: Vec::new() };
        let error =
            update_gains(&mut open, &settings, ProbeRole::First, &mut crate::motion::testing::FakeClock::default())
                .unwrap_err();
        assert_eq!(error.python_str(), "native reaction fixture");
        assert_eq!(*writes.lock().unwrap(), [b"[rdD][rdE][rdF][rdp]".to_vec()]);
    }

    #[test]
    fn native_reaction_maximum_adds_before_the_reached_wire_range_error() {
        let (mut driver, mut host, mut link) = connected_p3(Smc3Settings::default());
        driver.settings.update(|settings| {
            // The raw text as an integer of any width: 2**80.
            settings.native_reaction_speed = Some(Ok(LegacyValue::Str("1208925819614629174706176".into())));
            settings.update_kp = false;
        });
        stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
        let kp = (BigInt::from(1) << 80) + 20;
        assert_eq!(driver.previous_kp, kp);
        assert_eq!(writes(&mut host, "/dev/ttyUSB1").len(), 1);
        let error = gain_command(&native::big(kp.clone())).unwrap_err();
        assert_eq!(error.python_str(), "int too big to convert");
        assert!(!wire::gains_need_update_wide(&[], &kp, "kp"));
        assert!(wire::gains_need_update_wide(&gains(120), &kp, "kp"));
    }

    #[test]
    fn native_switch_error_keeps_two_box_stream_and_test_failure_prefixes() {
        for testing in [false, true] {
            let mut host = FakeHost::with_ports(&[])
                .device("/dev/left", Device::default())
                .device("/dev/right", Device::default());
            let mut driver = driver(Smc3Settings {
                left_controller_com_port: "/dev/left".into(),
                right_controller_com_port: "/dev/right".into(),
                ..Default::default()
            });
            let mut link = connect(&mut driver, &mut host, "P6");
            driver.settings.update(|settings| {
                settings.reaction_speed = 80;
                settings.native_switches = Some(NativeSmc3Switches {
                    enable_debug: Ok(false),
                    update_kp: Err("KeyError: 'update_kp'".into()),
                });
            });
            if testing {
                driver
                    .send_test(
                        &[2048; 6],
                        &mut MotionContext {
                            model: motion_model("P6", false).unwrap(),
                            link: &mut link,
                            host: &mut host,
                        },
                    )
                    .unwrap();
            } else {
                stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
            }
            assert_eq!(writes(&mut host, "/dev/left").len(), 1);
            assert_eq!(writes(&mut host, "/dev/right").len(), usize::from(!testing));
            assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
            assert!(driver.actuators_position().is_some());
            assert_eq!(driver.previous_kp, BigInt::from(100));
            assert_eq!(driver.port(BoxSlot::Left).is_some(), !testing);
            assert_eq!(driver.port(BoxSlot::Right).is_some(), !testing);
            if !testing {
                driver.settings.update(|settings| settings.native_switches.as_mut().unwrap().update_kp = Ok(true));
                stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
                assert_eq!(writes(&mut host, "/dev/left").len(), 2, "recovery adds one position frame without gains");
                driver.settings.update(|settings| settings.reaction_speed = 81);
                stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
                assert!(writes(&mut host, "/dev/left").contains(&gains(101)));
            }
        }
        let missing = MotionError::Native("KeyError: 'update_kp'".into());
        assert_eq!(exception_text(&missing), "'update_kp'");
        assert_eq!(exception_repr(&missing), "KeyError('update_kp')");
        let invalid = MotionError::Native("Exception: Boolean value expected. Got maybe".into());
        assert_eq!(exception_repr(&invalid), "Exception('Boolean value expected. Got maybe')");
    }

    #[test]
    fn native_debug_error_precedes_connect_and_frames_while_true_skips_gain_read() {
        let mut host = FakeHost::with_ports(&["/dev/left"]);
        let mut driver = driver(Smc3Settings::default());
        driver.settings.update(|settings| {
            settings.native_switches = Some(NativeSmc3Switches {
                enable_debug: Err("Exception: Boolean value expected. Got invalid".into()),
                update_kp: Err("KeyError: 'update_kp'".into()),
            })
        });
        let mut link = LinkState::default();
        let model = motion_model("P6", false).unwrap();
        let error = driver.connect(&mut MotionContext { model, link: &mut link, host: &mut host }).unwrap_err();
        assert_eq!(error.python_str(), "Boolean value expected. Got invalid");
        assert!(host.log("/dev/left").opens.is_empty());
        assert!(stream(&mut driver, &mut host, &mut link, &[]).is_err(), "debug read precedes motor indexing");
        assert!(host.datagrams.is_empty());
        driver.settings.update(|settings| settings.native_switches.as_mut().unwrap().enable_debug = Ok(true));
        driver.connect(&mut MotionContext { model, link: &mut link, host: &mut host }).unwrap();
        stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap();
        assert_eq!(host.datagrams.len(), 2);
        assert!(host.log("/dev/left").opens.is_empty());
    }

    #[test]
    fn native_scan_gain_switch_fails_after_identity_before_gain_queries() {
        let mut host = FakeHost::with_ports(&["/dev/left"]).device("/dev/left", dof_box(VER_10_BIT, 1, None));
        let mut driver = driver(Smc3Settings {
            native_switches: Some(NativeSmc3Switches {
                enable_debug: Ok(false),
                update_kp: Err("TypeError: fixture native switch".into()),
            }),
            ..Default::default()
        });
        let mut link = LinkState::default();
        let error = driver
            .connect(&mut MotionContext { model: motion_model("P3", false).unwrap(), link: &mut link, host: &mut host })
            .unwrap_err();
        assert_eq!(error.python_str(), "fixture native switch");
        let writes = writes(&mut host, "/dev/left");
        assert!(writes.contains(&b"[vmd]".to_vec()));
        assert!(writes.contains(&b"[v00]".to_vec()));
        assert!(!writes.contains(&b"[rdD][rdE][rdF][rdp]".to_vec()));
        assert!(link.ports.is_empty());
    }

    /// send_stream: a failed write marks `Not Connected` and keeps the port.
    #[test]
    fn send_stream_failure_marks_not_connected() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"])
            .device("/dev/ttyUSB1", Device { fail_writes: true, ..Device::default() });
        let mut d = driver(Smc3Settings { left_controller_com_port: "/dev/ttyUSB1".into(), ..Smc3Settings::default() });
        let mut link = connect(&mut d, &mut host, "P3");
        assert!(link.is_connected());
        stream(&mut d, &mut host, &mut link, &[2048; 6]).unwrap();
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(d.port(BoxSlot::Left).as_deref(), Some("/dev/ttyUSB1"));
        assert!(d.actuators_position().is_some(), "send_stream L721 publishes before the L732 write");
    }

    /// send_test: a failed write closes the box and forgets both slots.
    #[test]
    fn send_test_failure_closes_boxes() {
        let mut host =
            FakeHost::with_ports(&[]).device("/dev/ttyUSB1", Device { fail_writes: true, ..Device::default() });
        let mut d = driver(Smc3Settings {
            left_controller_com_port: "/dev/ttyUSB1".into(),
            right_controller_com_port: "/dev/ttyUSB2".into(),
            ..Smc3Settings::default()
        });
        let mut link = connect(&mut d, &mut host, "P6");
        let spec = motion_model("P6", false).unwrap();
        d.send_test(&[2048; 6], &mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap();
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(d.port(BoxSlot::Left), None);
        assert_eq!(d.port(BoxSlot::Right), None);
        assert!(host.log("/dev/ttyUSB2").writes.is_empty());
        assert_eq!(d.actuators_position(), None, "send_test L807–L808: no publish after a failed write");
    }

    /// send_test: the box-1 write comes first, then actuators_position is published.
    #[test]
    fn send_test_l807_l808_publishes_after_a_successful_write() {
        let (mut d, mut host, mut link) = connected_p3(Smc3Settings::default());
        let spec = motion_model("P3", false).unwrap();
        d.send_test(
            &[204, 2048, 3891, 2048, 0, 0],
            &mut MotionContext { model: spec, link: &mut link, host: &mut host },
        )
        .unwrap();
        assert_eq!(writes(&mut host, "/dev/ttyUSB1").len(), 1);
        let p = d.actuators_position().unwrap();
        assert_eq!((p.front_left, p.front_right, p.rear_left), (50, 511, 972));
    }

    /// send_stream: the frames index slots 0..=5.
    #[test]
    fn send_stream_needs_six_slots() {
        let (mut d, mut host, mut link) = connected_p3(Smc3Settings::default());
        let error = stream(&mut d, &mut host, &mut link, &[1, 2, 3]).unwrap_err();
        assert!(matches!(error.root(), MotionError::TooFewMotors { needed: 4, got: 3 }));
    }

    /// disconnect: close, `Not Connected`, then 1 s pause even without a box.
    #[test]
    fn disconnect_closes_and_pauses() {
        let (mut d, mut host, mut link) = connected_p3(Smc3Settings::default());
        let spec = motion_model("P3", false).unwrap();
        let before = host.now();
        d.disconnect(&mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap();
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(d.port(BoxSlot::Left), None);
        assert_eq!(host.now() - before, Duration::from_secs(1));
        link.status = LinkStatus::Connected;
        d.disconnect(&mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap();
        assert_eq!(link.status, LinkStatus::Connected, "nothing open: status untouched");
    }

    /// Connect: model `None` reports `Not Connected` without scanning.
    #[test]
    fn python_text_forms_of_logged_values() {
        assert_eq!(py::str_repr("/dev/ttyACM0"), "'/dev/ttyACM0'");
        assert_eq!(py::str_repr("it's"), "\"it's\"");
        assert_eq!(py::bytes_repr(b"[v\x00\xd2]"), "b'[v\\x00\\xd2]'");
        assert_eq!(py::bytes_repr(b"DOF\r\nv1"), "b'DOF\\r\\nv1'");
        assert_eq!(py::bytes_repr(b""), "b''");
        assert_eq!(py::hex(b"[v\x00\xd2]"), "5b7600d25d");
        assert_eq!(py::round2(1.0), "1.0");
        assert_eq!(py::round2(1.049_999), "1.05");
        assert_eq!(py::round2(10.004), "10.0");
        assert_eq!(py::round2(0.35), "0.35");
    }

    /// A port that cannot be opened: `SerialException(errno, "could not open port <p>: [Errno n] <strerror>:
    /// '<p>'")`, whose message adds `[Errno n] ` again.
    #[test]
    fn open_failure_exception_forms() {
        let busy = LinkError::Io(std::io::Error::from_raw_os_error(16));
        let (repr, text, errno) = open_exception("/dev/ttyACM0", &busy);
        assert_eq!(
            repr,
            "SerialException(16, \"could not open port /dev/ttyACM0: [Errno 16] Device or resource busy: \
             '/dev/ttyACM0'\")"
        );
        assert_eq!(
            text,
            "[Errno 16] could not open port /dev/ttyACM0: [Errno 16] Device or resource busy: '/dev/ttyACM0'"
        );
        assert_eq!(errno, Some(16));
        assert_eq!(errno_of("No such file or directory"), Some(2));
        assert_eq!(errno_of("not an os error"), None);
    }

    #[test]
    fn listed_port_description_and_hwid() {
        let mut host = FakeHost::default();
        let usb = PortInfo {
            path: "/dev/ttyUSB0".into(),
            stable_path: None,
            usb: Some(crate::discovery::UsbIdentity {
                vendor_id: 0x1a86,
                product_id: 0x7523,
                serial_number: Some("A1".into()),
                manufacturer: None,
                product: Some("USB Serial".into()),
            }),
        };
        assert_eq!(port_desc_hwid(&usb, &mut host), ("USB Serial".into(), "USB VID:PID=1A86:7523 SER=A1".into()));
        let mut details = crate::discovery::ListPortInfo::from_listing(&usb);
        details.interface = Some("Controller A".into());
        details.location = Some("1-1:1.0".into());
        details.apply_usb_info();
        host.listed_details.insert(usb.path.clone(), details);
        assert_eq!(
            port_desc_hwid(&usb, &mut host),
            ("USB Serial - Controller A".into(), "USB VID:PID=1A86:7523 SER=A1 LOCATION=1-1:1.0".into())
        );
        let plain = PortInfo { path: "/dev/ttyS0".into(), stable_path: None, usb: None };
        assert_eq!(port_desc_hwid(&plain, &mut host), ("n/a".into(), "n/a".into()));
    }

    #[test]
    fn native_pnp_diagnostics_keep_raw_none_in_scan_tuple_and_text_in_print_fields() {
        let mut host = FakeHost::with_ports(&["/dev/ttyS0"]);
        let port = host.ports[0].clone();
        let mut details = crate::discovery::ListPortInfo::from_listing(&port);
        details.hwid = None;
        host.listed_details.insert(port.path.clone(), details);
        assert_eq!(port_desc_hwid(&port, &mut host).1, "None");
        let mut link = LinkState::default();
        let messages = crate::motion::driver_support::testing::capture_debug_messages(|| {
            let mut ctx = MotionContext { model: motion_model("P3", false).unwrap(), link: &mut link, host: &mut host };
            log_scan_start(&[port], &mut ctx);
        });
        assert!(messages[0].contains("ports=[('/dev/ttyS0', 'n/a', None)]"), "{messages:?}");
        assert!(!messages[0].contains("'None'"));
        assert!(host.log("/dev/ttyS0").opens.is_empty());
    }

    /// Connect: the Kp written is the integer reaction speed plus 20, read after the `[rdD..]` reply, so a change
    /// made while the box answers is the one written.
    #[test]
    fn connect_l420_kp_is_read_after_the_gain_reply() {
        let settings = LiveSettings::new(Smc3Settings::default());
        let written = Arc::new(Mutex::new(Vec::new()));
        let link = ChangingLink { settings: settings.clone(), written: Arc::clone(&written), reply: Vec::new() };
        let mut open = SerialObject { port: "/dev/ttyUSB1".into(), link: Some(Box::new(link)), inbox: Vec::new() };
        let mut clock = crate::motion::testing::FakeClock::default();
        update_gains(&mut open, &settings, ProbeRole::First, &mut clock).unwrap();
        let expected = repeated(b"DEFp", 90 + KP_OFFSET).unwrap();
        assert!(written.lock().unwrap().contains(&expected), "Kp of reaction speed 90 written");
    }

    /// A box that answers the Kp read with zero gains; that read changes the reaction speed to 90.
    struct ChangingLink {
        settings: LiveSettings<Smc3Settings>,
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        reply: Vec<u8>,
    }

    impl Link for ChangingLink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            if bytes.starts_with(b"[rdD]") {
                self.settings.update(|s| s.reaction_speed = 90);
                self.reply = b"[D\x00\x00][E\x00\x00][F\x00\x00][p\x00\x00]".to_vec();
            }
            self.written.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
            Ok(std::mem::take(&mut self.reply))
        }
        fn clear_input(&mut self) -> Result<(), LinkError> {
            Ok(())
        }
        fn name(&self) -> &str {
            "/dev/ttyUSB1"
        }
    }

    #[test]
    fn none_model_reports_not_connected() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]);
        let mut d = driver(Smc3Settings::default());
        let mut link = LinkState { status: LinkStatus::Connected, ..LinkState::default() };
        let spec = motion_model("None", false).unwrap();
        d.connect(&mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap();
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert!(host.log("/dev/ttyUSB1").opens.is_empty());
    }

    /// Diagnostics: `[A fb/4 tgt/4]` replies on the 0..=4095 scale.
    #[test]
    fn feedback_parsing_takes_the_newest_frame() {
        let data = b"[A\x10\x20][B\x01\x02][A\x40\x80]";
        assert_eq!(diagnostics::latest_feedback(data, b'A'), Some((256 * 4095 / 1023, 512 * 4095 / 1023)));
        assert_eq!(diagnostics::latest_feedback(data, b'C'), None);
    }

    /// The starting Kp with a stream frame: a reaction speed changed after start-up
    /// but before the first frame is pushed with that frame. Runs through the registry.
    #[test]
    fn registry_drives_smc3_and_pushes_kp_changed_since_start() {
        use crate::motion::{ConnectPassInputs, DriverEnvironment, DriverKind, MotionRegistry};
        let settings = LiveSettings::new(Smc3Settings::default());
        let mut registry = MotionRegistry::new(DriverEnvironment::srs_linux());
        registry.register(DriverKind::Smc3, Smc3Driver::factory(settings.clone()));
        settings.update(|s| s.reaction_speed = 70);
        let mut device = dof_box(VER_10_BIT, 1, None);
        device.replies[3].1 = gains(90);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", device);
        let inputs = ConnectPassInputs { model: "P3", motion_switch_on: true, auto_search: true, enable_debug: false };
        registry.connect_pass(&inputs, &mut host);
        assert!(registry.link().is_connected());
        assert_eq!(
            registry.identity("P3").unwrap().model_id,
            Some(1),
            "parameters['motion_id'] reachable through the registry"
        );
        host.log("/dev/ttyUSB1").writes.clear();
        registry.send_stream("P3", &[2048; 12], &mut host).unwrap();
        let w = writes(&mut host, "/dev/ttyUSB1");
        assert_eq!(w.len(), 3);
        assert_eq!(w[1], gains(90));
        assert_eq!(registry.actuators_position("P3").unwrap().motors[0], 511);
    }

    /// A host whose ports open exclusively, like the
    /// serialport crate's TIOCEXCL on Linux: a port cannot be opened again while an earlier link to it is alive.
    struct ExclusiveHost {
        inner: FakeHost,
        held: Arc<Mutex<Vec<String>>>,
        /// Opens refused because the port was held.
        refused: Vec<String>,
        /// Every write of every link fails while set (a box that stopped answering).
        writes_fail: Arc<Mutex<bool>>,
    }

    struct ExclusiveLink {
        inner: Box<dyn Link>,
        path: String,
        held: Arc<Mutex<Vec<String>>>,
        writes_fail: Arc<Mutex<bool>>,
    }

    impl Drop for ExclusiveLink {
        fn drop(&mut self) {
            let mut held = self.held.lock().unwrap();
            if let Some(i) = held.iter().position(|p| *p == self.path) {
                held.remove(i);
            }
        }
    }

    impl Link for ExclusiveLink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            if *self.writes_fail.lock().unwrap() {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "Write timeout").into());
            }
            self.inner.write_all(bytes)
        }
        fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
            self.inner.read_available()
        }
        fn clear_input(&mut self) -> Result<(), LinkError> {
            self.inner.clear_input()
        }
        fn name(&self) -> &str {
            self.inner.name()
        }
    }

    impl ExclusiveHost {
        fn new(inner: FakeHost) -> Self {
            ExclusiveHost { inner, held: Arc::default(), refused: Vec::new(), writes_fail: Arc::default() }
        }

        fn is_held(&self, path: &str) -> bool {
            self.held.lock().unwrap().iter().any(|p| p == path)
        }
    }

    impl MotionHost for ExclusiveHost {
        fn clock(&mut self) -> &mut dyn Clock {
            self.inner.clock()
        }
        fn serial_ports(&mut self) -> Vec<PortInfo> {
            self.inner.serial_ports()
        }
        fn open_serial(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
            if self.is_held(path) {
                self.refused.push(path.to_string());
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Access is denied.").into());
            }
            let inner = self.inner.open_serial(path, options)?;
            self.held.lock().unwrap().push(path.to_string());
            Ok(Box::new(ExclusiveLink {
                inner,
                path: path.to_string(),
                held: Arc::clone(&self.held),
                writes_fail: Arc::clone(&self.writes_fail),
            }))
        }
        /// A shared open: no lock, so it neither checks nor takes the hold.
        fn open_serial_shared(&mut self, path: &str, options: &OpenOptions) -> Result<Box<dyn Link>, LinkError> {
            let inner = self.inner.open_serial_shared(path, options)?;
            Ok(Box::new(ExclusiveLink {
                inner,
                path: String::new(),
                held: Arc::clone(&self.held),
                writes_fail: Arc::clone(&self.writes_fail),
            }))
        }
        fn claimed_ports(&self) -> crate::discovery::ClaimedPorts {
            self.inner.claimed_ports()
        }
        fn allowed_com_ports(&self) -> String {
            self.inner.allowed_com_ports()
        }
        fn send_datagram(&mut self, target: SocketAddr, payload: &[u8]) -> std::io::Result<()> {
            self.inner.send_datagram(target, payload)
        }
    }

    fn connect_on(driver: &mut Smc3Driver, host: &mut dyn MotionHost, model: &str, link: &mut LinkState) {
        let spec = motion_model(model, false).unwrap();
        driver.connect(&mut MotionContext { model: spec, link, host }).unwrap();
    }

    fn stream_on(driver: &mut Smc3Driver, host: &mut dyn MotionHost, model: &str, link: &mut LinkState) {
        let spec = motion_model(model, false).unwrap();
        driver.send_stream(&[2048; 12], &mut MotionContext { model: spec, link, host }).unwrap();
    }

    /// A stream frame keeps the object of slot 0 after a failed write; the next connect cannot reopen the held
    /// port, so it closes the latest first-box probe — the same object — and the pass ends `Not
    /// Connected`; the pass after that opens the port again and connects.
    #[test]
    fn connect_l311_l324_held_port_reconnects_on_the_second_pass() {
        let mut host = ExclusiveHost::new(
            FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_10_BIT, 1, None)),
        );
        let mut d = driver(Smc3Settings::default());
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        assert!(link.is_connected());
        *host.writes_fail.lock().unwrap() = true;
        stream_on(&mut d, &mut host, "P3", &mut link);
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert!(d.is_open(BoxSlot::Left), "send_stream does not close the port");

        *host.writes_fail.lock().unwrap() = false;
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(host.refused, ["/dev/ttyUSB1"]);
        assert!(!d.is_open(BoxSlot::Left), "L324 closed the held object");
        assert_eq!(d.port(BoxSlot::Left).as_deref(), Some("/dev/ttyUSB1"), "controller[0] still names it");

        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        assert!(link.is_connected());
        assert!(d.is_open(BoxSlot::Left));
    }

    /// Connect: a configured port still held by the object of slot 0 cannot be reopened; the failure handling
    /// closes the held object and ends the connect. The next pass opens the port.
    #[test]
    fn connect_l234_l238_held_configured_port_is_closed_then_reopened() {
        let mut host = ExclusiveHost::new(FakeHost::with_ports(&[]));
        let mut d = driver(Smc3Settings { left_controller_com_port: "/dev/ttyUSB1".into(), ..Smc3Settings::default() });
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        *host.writes_fail.lock().unwrap() = true;
        stream_on(&mut d, &mut host, "P3", &mut link);
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        *host.writes_fail.lock().unwrap() = false;

        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        assert_eq!(link.status, LinkStatus::NotConnected);
        assert!(link.ports.is_empty());
        assert!(!host.is_held("/dev/ttyUSB1"));

        connect_on(&mut d, &mut host, "P3", &mut link);
        assert!(link.is_connected());
        assert_eq!(link.ports[&0], "/dev/ttyUSB1");
    }

    /// Connect: every filled slot of the controller list reports its configured port, also an object kept from
    /// an earlier connection (here box 1, while only the right port is configured now).
    #[test]
    fn connect_l266_l271_kept_objects_report_the_configured_ports() {
        let mut host = ExclusiveHost::new(FakeHost::with_ports(&[]));
        let settings = LiveSettings::new(Smc3Settings {
            left_controller_com_port: "/dev/ttyUSB1".into(),
            ..Smc3Settings::default()
        });
        let mut d = Smc3Driver::new(settings.clone());
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P6", &mut link);
        settings.update(|s| {
            s.left_controller_com_port.clear();
            s.right_controller_com_port = "/dev/ttyUSB2".into();
        });
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P6", &mut link);
        assert!(link.is_connected());
        assert_eq!(link.ports[&0], "", "DOF.left_controller_com_port");
        assert_eq!(link.ports[&1], "/dev/ttyUSB2");
        assert!(d.is_open(BoxSlot::Left), "the old box-1 object stays open");
    }

    /// Connect: a second box with an unknown number stays the latest second-box probe
    /// (open). When the outer scan reaches that port, the open fails and closes the latest first-box probe, which
    /// is the placed box 1.
    #[test]
    fn connect_l324_open_failure_closes_the_previous_first_controller() {
        let mut host = ExclusiveHost::new(
            FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
                .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(1)))
                .device("/dev/ttyUSB2", dof_box(VER_10_BIT, 6, Some(7))),
        );
        let mut d = driver(Smc3Settings::default());
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P6", &mut link);
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(host.refused, ["/dev/ttyUSB2"]);
        assert_eq!(link.ports[&0], "/dev/ttyUSB1");
        assert!(!d.is_open(BoxSlot::Left), "box 1 was closed by the open failure on USB2");
        assert!(host.is_held("/dev/ttyUSB2"), "second_controller still holds the unnumbered box");
    }

    /// A failed test frame closes the object of slot 0 and forgets both slots; an object that is also
    /// the latest second-box probe stays open, one only the list named is closed with the list.
    #[test]
    fn send_test_l841_l864_reset_keeps_objects_named_by_the_probe_globals() {
        let mut host = ExclusiveHost::new(
            FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"])
                .device("/dev/ttyUSB1", dof_box(VER_10_BIT, 6, Some(1)))
                .device("/dev/ttyUSB2", dof_box(VER_10_BIT, 6, Some(2))),
        );
        let mut d = driver(Smc3Settings::default());
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P6", &mut link);
        assert!(link.is_connected());
        // Box 1 is in slot 0 and is the latest first-box probe, box 2 is in slot 1 and is the latest second-box probe.
        d.controller[0].as_ref().unwrap().get().close();
        let spec = motion_model("P6", false).unwrap();
        d.send_test(&[2048; 12], &mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap();
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert_eq!(d.port(BoxSlot::Left), None);
        assert_eq!(d.port(BoxSlot::Right), None);
        assert!(host.is_held("/dev/ttyUSB2"), "second_controller keeps box 2 open");
    }

    /// Disconnect: a model with fewer than 6 motors closes slot 0 only.
    #[test]
    fn disconnect_l886_l892_closes_box_one_only() {
        let mut host = ExclusiveHost::new(FakeHost::with_ports(&[]));
        let mut d = driver(Smc3Settings {
            left_controller_com_port: "/dev/ttyUSB1".into(),
            right_controller_com_port: "/dev/ttyUSB2".into(),
            ..Smc3Settings::default()
        });
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        let spec = motion_model("P3", false).unwrap();
        d.disconnect(&mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap();
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        assert!(!host.is_held("/dev/ttyUSB1"));
        assert!(!host.is_held("/dev/ttyUSB2"), "reset_controller drops the last reference to box 2");
    }

    fn linux_build() -> Smc3Settings {
        Smc3Settings { pre_connection_reset: true, exclusive_open: false, ..Smc3Settings::default() }
    }

    /// With shared opens there is no lock: the pre-reset and the probe open shared, and a port the
    /// object of the failed connection still holds opens again on the first reconnect pass.
    #[test]
    fn connect_l301_l311_linux_build_reopens_a_held_port_at_once() {
        let mut host = ExclusiveHost::new(
            FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_10_BIT, 1, None)),
        );
        let mut d = driver(linux_build());
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        assert!(link.is_connected());
        *host.writes_fail.lock().unwrap() = true;
        stream_on(&mut d, &mut host, "P3", &mut link);
        assert_eq!(link.status, LinkStatus::NotConnectedAfterWriteError);
        *host.writes_fail.lock().unwrap() = false;
        let mut link = LinkState::default();
        connect_on(&mut d, &mut host, "P3", &mut link);
        assert!(link.is_connected());
        assert!(host.refused.is_empty());
        assert!(host.inner.log("/dev/ttyUSB1").shared.iter().all(|&shared| shared));
    }

    /// The default settings open configured and scanned ports exclusively.
    #[test]
    fn connect_l234_l311_windows_build_opens_exclusively() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_10_BIT, 1, None));
        let mut d = driver(Smc3Settings::default());
        assert!(connect(&mut d, &mut host, "P3").is_connected());
        assert_eq!(host.log("/dev/ttyUSB1").shared, [false]);
        let mut d = driver(Smc3Settings { left_controller_com_port: "/dev/ttyUSB2".into(), ..Smc3Settings::default() });
        assert!(connect(&mut d, &mut host, "P3").is_connected());
        assert_eq!(host.log("/dev/ttyUSB2").shared, [false]);
    }

    /// The bring-up probe opens with the default settings: no pre-connection reset, one exclusive
    /// open; with the pre-reset and shared opens enabled it runs the pre-reset and opens shared.
    #[test]
    fn identify_port_follows_the_reproduced_build() {
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_12_BIT, 1, None));
        let id = identify_port("/dev/ttyUSB1", &mut host).unwrap();
        assert_eq!((id.version.version, id.model_id), (210, Some(1)));
        assert_eq!(host.log("/dev/ttyUSB1").shared, [false]);
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1"]).device("/dev/ttyUSB1", dof_box(VER_12_BIT, 1, None));
        identify_port_with("/dev/ttyUSB1", &linux_build(), &mut host).unwrap();
        let log = host.log("/dev/ttyUSB1");
        assert_eq!(log.shared, [true, true]);
        assert_eq!(log.opens[0].write_timeout, None, "pre-reset");
    }

    /// The class-and-message form and the message of a failed frame write's errors (stream and test frames).
    #[test]
    fn send_failure_exception_text() {
        let timeout =
            MotionError::from(LinkError::from(std::io::Error::new(std::io::ErrorKind::TimedOut, "Write timeout")));
        assert_eq!(exception_repr(&timeout), "SerialTimeoutException('Write timeout')");
        assert_eq!(exception_text(&timeout), "Write timeout");
        let closed = MotionError::from(ClosedLink::new("/dev/ttyUSB1").write_all(b"x").unwrap_err());
        assert_eq!(exception_repr(&closed), "PortNotOpenError('Attempting to use a port that is not open')");
    }

    /// `(function, line)` of the frames recorded for an error, outermost first.
    fn trace(error: &MotionError) -> Vec<(&'static str, u32)> {
        error.frames().iter().map(|frame| (frame.function, frame.line)).collect()
    }

    fn switches(enable_debug: Result<bool, &str>, update_kp: Result<bool, &str>) -> Option<NativeSmc3Switches> {
        Some(NativeSmc3Switches {
            enable_debug: enable_debug.map_err(str::to_owned),
            update_kp: update_kp.map_err(str::to_owned),
        })
    }

    fn connect_error(driver: &mut Smc3Driver, host: &mut FakeHost, model: &str) -> MotionError {
        let mut link = LinkState::default();
        let model = motion_model(model, false).unwrap();
        driver.connect(&mut MotionContext { model, link: &mut link, host }).unwrap_err()
    }

    fn test_error(driver: &mut Smc3Driver, host: &mut FakeHost, link: &mut LinkState, motors: &[i64]) -> MotionError {
        let model = motion_model("P3", false).unwrap();
        driver.send_test(motors, &mut MotionContext { model, link, host }).unwrap_err()
    }

    /// The steps of a connect whose errors leave it: the debug switch,
    /// the `update_kp` switch for the first box and
    /// the integer reaction speed plus 20. A value that is not a boolean fails inside
    /// the boolean conversion's frame; a missing key fails at the frame line of the read.
    #[test]
    fn connect_setting_errors_carry_their_source_lines() {
        let mut host = FakeHost::with_ports(&["/dev/left"]).device("/dev/left", dof_box(VER_10_BIT, 1, None));
        let mut driver = driver(Smc3Settings {
            native_switches: switches(Err("Exception: Boolean value expected. Got invalid"), Ok(true)),
            ..Default::default()
        });
        let error = connect_error(&mut driver, &mut host, "P3");
        assert_eq!(
            error.python_traceback().as_deref(),
            Some(
                "Traceback (most recent call last):\n  File \"backend/controllers/motion_helpers.py\", line 100, in \
                 backend.controllers.motion_helpers.connect\n  File \"backend/controllers/smc3.py\", line 209, in \
                 backend.controllers.smc3.connect\n  File \"backend/functions.py\", line 261, in \
                 backend.functions.str2bool\nException: Boolean value expected. Got invalid\n"
            )
        );
        driver.settings.update(|s| s.native_switches = switches(Err("KeyError: 'enable_debug'"), Ok(true)));
        let error = connect_error(&mut driver, &mut host, "P3");
        assert_eq!(trace(&error), [("connect", 209)]);
        assert_eq!(error.exception_line().as_deref(), Some("KeyError: 'enable_debug'"));

        driver.settings.update(|s| s.native_switches = switches(Ok(false), Err("KeyError: 'DOF REALITY'")));
        assert_eq!(trace(&connect_error(&mut driver, &mut host, "P3")), [("connect", 410)]);
        driver.settings.update(|s| {
            s.native_switches = switches(Ok(false), Err("Exception: Boolean value expected. Got maybe"));
        });
        assert_eq!(trace(&connect_error(&mut driver, &mut host, "P3")), [("connect", 410), ("str2bool", 261)]);

        driver.settings.update(|s| {
            s.native_switches = None;
            s.native_reaction_speed = Some(Err("ValueError: invalid literal for int() with base 10: '80.5'".into()));
        });
        let error = connect_error(&mut driver, &mut host, "P3");
        assert_eq!(trace(&error), [("connect", 420)]);
        assert_eq!(
            error.exception_line().as_deref(),
            Some("ValueError: invalid literal for int() with base 10: '80.5'")
        );
    }

    /// The second box's gain update of a connect: `update_kp`; the `reaction_speed` read spans two
    /// frame lines, the lookup and the integer conversion; then the Kp command.
    #[test]
    fn second_box_setting_errors_carry_their_source_lines() {
        struct Replying(Vec<u8>);
        impl Link for Replying {
            fn write_all(&mut self, _: &[u8]) -> Result<(), LinkError> {
                Ok(())
            }
            fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
                Ok(self.0.clone())
            }
            fn clear_input(&mut self) -> Result<(), LinkError> {
                Ok(())
            }
            fn name(&self) -> &str {
                "fake second box"
            }
        }
        let failing = |change: fn(&mut Smc3Settings)| {
            let settings = LiveSettings::new(Smc3Settings::default());
            settings.update(change);
            let link: Box<dyn Link> = Box::new(Replying(gains(120)));
            let mut open = SerialObject { port: "fake".into(), link: Some(link), inbox: Vec::new() };
            let mut clock = crate::motion::testing::FakeClock::default();
            update_gains(&mut open, &settings, ProbeRole::Second, &mut clock).unwrap_err()
        };
        let error = failing(|s| s.native_reaction_speed = Some(Err("KeyError: 'reaction_speed'".into())));
        assert_eq!(trace(&error), [("connect", 578)]);
        let error = failing(|s| {
            s.native_reaction_speed = Some(Err("ValueError: invalid literal for int() with base 10: ''".into()));
        });
        assert_eq!(trace(&error), [("connect", 577)]);
        let error = failing(|s| s.reaction_speed = 70_000);
        assert_eq!(trace(&error), [("connect", 586), ("gen_smc3_command", 83)]);
        assert_eq!(error.exception_line().as_deref(), Some("OverflowError: int too big to convert"));

        let mut host =
            FakeHost::with_ports(&["/dev/left", "/dev/right"]).device("/dev/right", dof_box(VER_10_BIT, 6, Some(2)));
        let mut driver = driver(Smc3Settings {
            native_switches: switches(Ok(false), Err("KeyError: 'update_kp'")),
            ..Default::default()
        });
        let mut link = LinkState::default();
        let model = motion_model("P6", false).unwrap();
        let error = driver
            .find_second_box("/dev/left", &mut MotionContext { model, link: &mut link, host: &mut host })
            .unwrap_err();
        assert_eq!(trace(&error), [("connect", 567)]);
    }

    /// The Kp command of the first box packs into two big-endian bytes:
    /// the two overflow error messages (negative, and too large).
    #[test]
    fn connect_gain_overflow_is_the_to_bytes_error_inside_gen_smc3_command() {
        for (reaction_speed, message) in
            [(70_000, "int too big to convert"), (-21, "can't convert negative int to unsigned")]
        {
            let mut host = FakeHost::with_ports(&["/dev/left"]).device("/dev/left", dof_box(VER_10_BIT, 1, None));
            let mut driver = driver(Smc3Settings { reaction_speed, ..Default::default() });
            let error = connect_error(&mut driver, &mut host, "P3");
            assert_eq!(trace(&error), [("connect", 428), ("gen_smc3_command", 83)]);
            assert_eq!(error.python_str(), message);
            assert_eq!(error.exception_line(), Some(format!("OverflowError: {message}")));
        }
    }

    /// The frames of the setting reads of a stream frame and of a test frame.
    #[test]
    fn frame_setting_errors_carry_their_source_lines() {
        let (mut driver, mut host, mut link) = connected_p3(Smc3Settings::default());
        driver.settings.update(|s| s.native_reaction_speed = Some(Err("KeyError: 'reaction_speed'".into())));
        assert_eq!(trace(&stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap_err()), [("send_stream", 714)]);
        assert_eq!(trace(&test_error(&mut driver, &mut host, &mut link, &[2048; 6])), [("send_test", 800)]);
        driver.settings.update(|s| {
            s.native_switches = switches(Err("Exception: Boolean value expected. Got invalid"), Ok(true));
        });
        assert_eq!(
            trace(&stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap_err()),
            [("send_stream", 708), ("str2bool", 261)]
        );
        driver.settings.update(|s| s.native_switches = switches(Err("KeyError: 'MOTIONBOX'"), Ok(true)));
        assert_eq!(trace(&test_error(&mut driver, &mut host, &mut link, &[2048; 6])), [("send_test", 793)]);
        // A stream frame reads `reaction_speed` before it reads the list; a test frame after.
        driver.settings.update(|s| s.native_switches = None);
        assert_eq!(trace(&stream(&mut driver, &mut host, &mut link, &[]).unwrap_err()), [("send_stream", 714)]);
        assert_eq!(trace(&test_error(&mut driver, &mut host, &mut link, &[])), [("send_test", 797)]);
    }

    /// The box-1 frame (slots 0 to 3) is built before
    /// the box-2 frame (slots 3 to 5): a short list is an
    /// index error at the frame line that reads past it, and a value that does not fit two bytes fails at the
    /// packing line of its letter.
    #[test]
    fn frame_generation_errors_carry_the_statement_and_generator_lines() {
        let (mut driver, mut host, mut link) = connected_p3(Smc3Settings::default());
        driver.frame_resolution = [Resolution(16); 2];
        let error = stream(&mut driver, &mut host, &mut link, &[1, 2, 3]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 715)]);
        assert_eq!(error.exception_line().as_deref(), Some("IndexError: list index out of range"));
        let error = stream(&mut driver, &mut host, &mut link, &[1, 2, 3, 4, 5]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 716)]);
        assert_eq!(trace(&test_error(&mut driver, &mut host, &mut link, &[1, 2, 3])), [("send_test", 797)]);
        assert_eq!(trace(&test_error(&mut driver, &mut host, &mut link, &[1, 2, 3, 4, 5])), [("send_test", 798)]);

        let error = stream(&mut driver, &mut host, &mut link, &[0, 65_536, 0, 0, 0, 0]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 715), ("gen_abcy_command", 114)]);
        assert_eq!(error.exception_line().as_deref(), Some("OverflowError: int too big to convert"));
        let error = stream(&mut driver, &mut host, &mut link, &[0, 0, 0, 0, 0, -1]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 716), ("gen_abc_command", 96)]);
        assert_eq!(error.exception_line().as_deref(), Some("OverflowError: can't convert negative int to unsigned"));
        // The first frame has already failed when the second would read past the list.
        let error = test_error(&mut driver, &mut host, &mut link, &[-1, 0, 0, 0, 0]);
        assert_eq!(trace(&error), [("send_test", 797), ("gen_abcy_command", 113)]);
        assert!(writes(&mut host, "/dev/ttyUSB1").is_empty());

        // The UDP mirror (of a stream or test frame) builds the same frames at its own frame lines.
        driver.settings.update(|s| s.enable_debug = true);
        let error = stream(&mut driver, &mut host, &mut link, &[0, 0, 0, 65_536, 0, 0]).unwrap_err();
        assert_eq!(trace(&error), [("send_stream", 709), ("debug", 899), ("gen_abcy_command", 116)]);
        let error = test_error(&mut driver, &mut host, &mut link, &[0, 0, 0, 0, 65_536, 0]);
        assert_eq!(trace(&error), [("send_test", 794), ("debug", 900), ("gen_abc_command", 95)]);
        assert_eq!(trace(&driver.debug(&[0; 5], &mut host).unwrap_err()), [("debug", 900)]);
    }

    /// A gain value that does not fit two bytes is handled with the frame's write errors: a stream frame
    /// logs its class-and-message form.
    #[test]
    fn caught_gain_overflow_logs_the_to_bytes_error() {
        use crate::motion::driver_support::testing::capture_debug_messages;
        let (mut driver, mut host, mut link) = connected_p3(Smc3Settings::default());
        driver.settings.update(|s| s.reaction_speed = 70_000);
        let logged = capture_debug_messages(|| stream(&mut driver, &mut host, &mut link, &[2048; 6]).unwrap());
        assert_eq!(
            logged,
            ["SMC3 send_stream failed on controller[0] | port=/dev/ttyUSB1 | is_open=True | \
              error=OverflowError('int too big to convert') | errno=None | winerror=None"]
        );
        assert_eq!(
            exception_text(&gain_command(&native::int(-1)).unwrap_err()),
            "can't convert negative int to unsigned"
        );
    }

    fn native_list(values: Vec<LegacyValue>) -> LegacyValue {
        LegacyValue::Object(openrigcue_model::native_object::NativeObject::new(LegacyValue::List(values)))
    }

    /// On a 10-bit box every value is first rescaled from 0..4095 to 0..1023: a slot of any width
    /// that a 64-bit float holds is clamped by the rescale (2**80 sends 1023, a negative one 0); one too wide for
    /// a float fails inside the rescale, through the position command's scaling line, before any packing or write.
    #[test]
    fn native_ten_bit_scaling_converts_each_slot_to_a_double_first() {
        let (mut d, mut host, mut link) = connected_p3(Smc3Settings::default());
        let spec = motion_model("P3", false).unwrap();
        let wide = LegacyValue::from_integer(BigInt::from(1) << 80);
        let mut list =
            native_list(vec![wide, native::int(-5), native::int(4095), native::int(0), native::int(0), native::int(0)]);
        d.send_stream_native(&mut list, &mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap();
        assert_eq!(
            writes(&mut host, "/dev/ttyUSB1"),
            [positions(b"ABCY", &[4095, 0, 4095, 0], Resolution::TEN_BIT).unwrap()]
        );
        let huge = LegacyValue::from_integer(BigInt::from(1) << 1100);
        let mut list =
            native_list(vec![native::int(0), huge, native::int(0), native::int(0), native::int(0), native::int(0)]);
        let error = d
            .send_stream_native(&mut list, &mut MotionContext { model: spec, link: &mut link, host: &mut host })
            .unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "OverflowError: int too large to convert to float");
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 715), ("gen_abcy_command", 107), ("map", 204)]);
        assert_eq!(writes(&mut host, "/dev/ttyUSB1").len(), 1, "nothing is written for the failed frame");
    }

    /// A box whose resolution is not 10 packs the slot itself into two big-endian bytes, at any width.
    #[test]
    fn native_unscaled_box_packs_with_to_bytes() {
        let mut d = driver(Smc3Settings::default());
        d.frame_resolution = [Resolution::TWELVE_BIT, Resolution::TWELVE_BIT];
        let wide = LegacyValue::from_integer(BigInt::from(1) << 80);
        let list =
            native_list(vec![native::int(1), wide, native::int(3), native::int(4), native::int(5), native::int(6)]);
        let error = d.frames(&list, "send_stream", [715, 716]).unwrap_err();
        assert_eq!(error.python_str(), "int too big to convert");
        let frames: Vec<_> = error.frames().iter().map(|f| (f.function, f.line)).collect();
        assert_eq!(frames, [("send_stream", 715), ("gen_abcy_command", 114)]);
        let list = native_list((1..=6).map(native::int).collect());
        let [first, second] = d.frames(&list, "send_stream", [715, 716]).unwrap();
        assert_eq!(first, positions(b"ABCY", &[1, 2, 3, 4], Resolution::TWELVE_BIT).unwrap());
        assert_eq!(second, positions(b"ABC", &[4, 5, 6], Resolution::TWELVE_BIT).unwrap());
    }

    /// The UDP mirror on the dynamic list mirrors both frames; a short list fails at its first frame line.
    #[test]
    fn native_debug_mirrors_the_native_frames() {
        let d = driver(Smc3Settings::default());
        let mut host = FakeHost::default();
        let list = native_list((0..6).map(|_| native::int(2048)).collect());
        d.debug_native(&list, &mut host).unwrap();
        assert_eq!(host.datagrams.len(), 2);
        let error = d.debug_native(&native_list(vec![native::int(0)]), &mut host).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "IndexError: list index out of range");
        assert_eq!(error.frames().iter().map(|f| (f.function, f.line)).collect::<Vec<_>>(), [("debug", 899)]);
    }

    /// A connect reads the `DOF REALITY` settings at each access: a live edit after the refresh is what the
    /// next reconnect opens (typed values empty here), and a resolution that is not an integer falls back to 10.
    #[test]
    fn native_configured_ports_and_resolutions_are_read_live_at_connect() {
        use crate::motion::driver_support::testing::FakeConfig;
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"]);
        let settings = Smc3Settings::default();
        let config = std::sync::Arc::new(FakeConfig::default());
        settings.runtime.install_config(config.clone());
        for (key, value) in [("left_controller_com_port", ""), ("right_controller_com_port", "")] {
            config.set("DOF REALITY", key, LegacyValue::from(value));
        }
        config.set("DOF REALITY", "left_controller_resolution", LegacyValue::from_integer(BigInt::from(1) << 80));
        config.set("DOF REALITY", "right_controller_resolution", LegacyValue::from("x"));
        let mut d = driver(settings);
        config.set("DOF REALITY", "left_controller_com_port", LegacyValue::from("/dev/ttyUSB2"));
        let link = connect(&mut d, &mut host, "P3");
        assert!(link.is_connected());
        assert_eq!(link.ports[&0], "/dev/ttyUSB2");
        assert!(host.log("/dev/ttyUSB1").opens.is_empty(), "no scan: the live port was configured");
        assert_eq!(d.frame_resolution(), [Resolution(i64::MAX), Resolution::TEN_BIT], "2**80 is not 10");
        // A key removed from the live tree is an error at the frame line that reads it.
        config.0.lock().unwrap().remove(&("DOF REALITY".into(), "left_controller_com_port".into()));
        let spec = motion_model("P3", false).unwrap();
        let mut link = LinkState::default();
        let error = d.connect(&mut MotionContext { model: spec, link: &mut link, host: &mut host }).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "KeyError: 'left_controller_com_port'");
    }

    /// The errors of the open are handled: a port read that fails there takes the
    /// failure path (message, port entry dropped, the connect ends quietly) instead of ending the connect with
    /// an error; the earlier emptiness tests are not handled.
    #[test]
    fn native_open_statement_port_failure_enters_the_open_handler() {
        use crate::motion::driver_support::{ConfigSource, RuntimeReader};
        struct ThirdReadFails(std::sync::atomic::AtomicUsize);
        impl ConfigSource for ThirdReadFails {
            fn value(&self, _section: &str, key: &str) -> Result<LegacyValue, String> {
                match key {
                    "left_controller_com_port" => {
                        let read = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if read == 2 {
                            Err("KeyError: 'left_controller_com_port'".into())
                        } else {
                            Ok("/dev/ttyUSB2".into())
                        }
                    }
                    "right_controller_com_port" => Ok("".into()),
                    _ => Err(format!("KeyError: '{key}'")),
                }
            }
            fn switch(&self, _section: &str, _key: &str) -> Result<bool, String> {
                Ok(false)
            }
            fn filter(&self, _section: &str) -> Result<bool, String> {
                Ok(true)
            }
        }
        let runtime = RuntimeReader::default();
        let source = std::sync::Arc::new(ThirdReadFails(Default::default()));
        runtime.install_config(source.clone());
        let mut host = FakeHost::with_ports(&["/dev/ttyUSB1", "/dev/ttyUSB2"]);
        let mut d = driver(Smc3Settings { runtime, ..Smc3Settings::default() });
        let link = connect(&mut d, &mut host, "P3");
        assert!(!link.is_connected());
        assert!(host.log("/dev/ttyUSB2").opens.is_empty(), "the failed read precedes the open");
        assert!(host.log("/dev/ttyUSB1").opens.is_empty(), "the handler returns before the scan");
        assert_eq!(
            source.0.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "L218, L223, L227, then the handler's message"
        );
    }
}

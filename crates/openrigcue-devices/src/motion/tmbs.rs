//! The Dyadic Systems SCN-series "TMBS" ASCII bucket protocol.
//!
//! The client for the SCN6 actuators, the PROSIMU traction-loss/surge axes
//! and SimTech Revolution. Driver adapters share this framing and stream reader.
//! Only the commands those drivers send are implemented.
//!
//! Request: `STX <axis hex digit> <11-char command> <BCC 2 hex> ETX`. Reply: `STX 'U' <axis> <payload> <BCC>
//! ETX`, where the BCC covers `U`, the axis character and the payload.

use std::collections::VecDeque;
use std::time::Duration;

use tracing::debug;

use crate::motion::clock::Clock;
use crate::motion::{python, python_print, PyFrame};
use crate::serial::Link;

pub const STX: u8 = 0x02;
pub const ETX: u8 = 0x03;
/// The read timeout, applied to each read up to a delimiter.
pub const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// The write timeout.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// Shorter replies are rejected ("looks like its using the wrong baud rate").
const MIN_REPLY_LEN: usize = 16;

/// Block check characters: two upper-case hex digits of the two's complement of the byte sum.
pub fn bcc(body: &[u8]) -> [u8; 2] {
    let sum = body.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    let value = (sum ^ 0xFF).wrapping_add(1);
    let text = format!("{value:02X}");
    let bytes = text.as_bytes();
    [bytes[0], bytes[1]]
}

/// A request frame for `axis` (0–15): `STX`, the axis as a hex digit, the message, its block check
/// characters, `ETX`.
pub fn pack(axis: u8, command: &str) -> Vec<u8> {
    pack_axis(u32::from(axis), command)
}

/// SimTech's interface also accepts axes wider than a byte.
pub(crate) fn pack_axis(axis: u32, command: &str) -> Vec<u8> {
    let mut body = format!("{axis:x}").into_bytes();
    body.extend_from_slice(command.as_bytes());
    let check = bcc(&body);
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.push(STX);
    frame.extend_from_slice(&body);
    frame.extend_from_slice(&check);
    frame.push(ETX);
    frame
}

/// Why a reply was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnpackError {
    #[error("message size {0} less than expected, looks like its using the wrong baud rate")]
    TooShort(usize),
    #[error("bcc wrong")]
    BadCheck,
    #[error(transparent)]
    NotText(Utf8DecodeError),
}

/// The failure of decoding the payload, whose offsets refer to payload bytes rather than the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utf8DecodeError {
    payload: Vec<u8>,
    start: usize,
    end: usize,
    reason: &'static str,
}

impl Utf8DecodeError {
    fn from_error(payload: &[u8], error: std::str::Utf8Error) -> Self {
        let start = error.valid_up_to();
        let end = error.error_len().map_or(payload.len(), |length| start + length);
        let reason = if error.error_len().is_none() {
            "unexpected end of data"
        } else if matches!(payload[start], 0xc2..=0xf4) {
            "invalid continuation byte"
        } else {
            "invalid start byte"
        };
        Self { payload: payload.to_vec(), start, end, reason }
    }

    fn caught_traceback(&self) -> String {
        format!(
            "Traceback (most recent call last):\n  File \"backend/controllers/pytbmscom.py\", line 79, in \
             backend.controllers.pytbmscom.TMBSCOMProtocol.readResponse\n  File \"backend/controllers/pytbmscom.py\", \
             line 119, in backend.controllers.pytbmscom.TMBSCOMProtocol.unpackMessage\nUnicodeDecodeError: {self}\n"
        )
    }
}

impl std::fmt::Display for Utf8DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.end == self.start + 1 {
            write!(
                formatter,
                "'utf-8' codec can't decode byte 0x{:02x} in position {}: {}",
                self.payload[self.start], self.start, self.reason
            )
        } else {
            write!(
                formatter,
                "'utf-8' codec can't decode bytes in position {}-{}: {}",
                self.start,
                self.end - 1,
                self.reason
            )
        }
    }
}

impl std::error::Error for Utf8DecodeError {}

/// The payload of a reply frame `STX 'U' axis payload BCC ETX`.
///
/// The BCC is recomputed over `U`, the axis character and the decoded payload.
pub fn unpack(message: &[u8]) -> Result<Vec<u8>, UnpackError> {
    if message.len() < MIN_REPLY_LEN {
        return Err(UnpackError::TooShort(message.len()));
    }
    let inner = &message[2..message.len() - 1];
    let axis = char::from(inner[0]);
    let rest = &inner[1..];
    let (payload, check) = rest.split_at(rest.len() - 2);
    let text = std::str::from_utf8(payload)
        .map_err(|error| UnpackError::NotText(Utf8DecodeError::from_error(payload, error)))?;
    let body = format!("U{axis}{text}");
    if bcc(body.as_bytes()) == check {
        Ok(payload.to_vec())
    } else {
        Err(UnpackError::BadCheck)
    }
}

/// `n` as lower-case hex, zero-padded to four characters including a leading `-`.
pub fn py_hex4(n: i64) -> String {
    if n < 0 {
        format!("-{:03x}", n.unsigned_abs())
    } else {
        format!("{n:04x}")
    }
}

/// The hexadecimal integer in a bytes slice: surrounding whitespace, an optional sign, an optional `0x`
/// prefix and single underscores between digits are accepted.
pub fn py_int_hex(text: &[u8]) -> Option<i64> {
    python::int_hex_bytes(text)
}

/// Registers read with `R4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Register {
    /// The model code.
    ModelCode,
    /// The serial number.
    SerialNumber,
    /// The homing pattern.
    HomingPattern,
    /// The STAT flags.
    Status,
    /// The ALRM flags.
    Alarm,
}

impl Register {
    pub const fn address(self) -> &'static str {
        match self {
            Register::ModelCode => "00006800",
            Register::SerialNumber => "00006801",
            Register::HomingPattern => "00000006",
            Register::Status => "00007403",
            Register::Alarm => "00007404",
        }
    }
}

/// The commands the drivers send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Register read: `R4<addr>0`.
    Read(Register),
    /// Servo on or off (`q`).
    Servo(bool),
    /// Internal status reset `03` (`r`) — what the alarm check sends.
    InternalStatusReset,
    /// Status check (`n`) — what the alarm reset sends.
    StatusCheck,
    /// Cancellation of the remaining movement stroke (`d`).
    CancelMovement,
    /// Homing movement: `o<mode>00000000`.
    Homing(String),
    /// Speed and acceleration update (`v`, mode `2`), each as four lower-case hex digits.
    Velocity { speed: i64, accel: i64 },
    /// [`Command::Velocity`] with the two hex fields already formatted (values of any width).
    VelocityHex { speed: String, accel: String },
    /// Point-to-point movement (`a`): absolute move to a signed 32-bit increment count.
    MoveAbsolute(i32),
}

impl Command {
    /// The command text (11 characters for in-range arguments).
    pub fn text(&self) -> String {
        match self {
            Command::Read(register) => read_register_command(register.address()),
            Command::Servo(on) => format!("q{}000000000", if *on { '1' } else { '0' }),
            Command::InternalStatusReset => "r0300000000".into(),
            Command::StatusCheck => "n0000000000".into(),
            Command::CancelMovement => "d0000000000".into(),
            Command::Homing(mode) => format!("o{mode}00000000"),
            Command::Velocity { speed, accel } => {
                format!("v2{}{}0", py_hex4(*speed), py_hex4(*accel))
            }
            Command::VelocityHex { speed, accel } => format!("v2{speed}{accel}0"),
            // The position as eight upper-case hex digits of its big-endian signed 32-bit form.
            Command::MoveAbsolute(position) => format!("a{:08X}00", *position as u32),
        }
    }
}

pub fn read_register_command(address: &str) -> String {
    format!("R4{address}0")
}

impl Command {
    /// The name of this command's operation, which its `'<name> - failed'` print uses.
    pub fn method_name(&self) -> &'static str {
        match self {
            Command::Read(register) => register.method_name(),
            Command::Servo(true) => "set_son",
            Command::Servo(false) => "set_soff",
            Command::InternalStatusReset => "check_alrm",
            Command::StatusCheck => "reset_alarm",
            Command::CancelMovement => "cancel_movement",
            Command::Homing(_) => "move_org",
            Command::Velocity { .. } | Command::VelocityHex { .. } => "write_velocity",
            Command::MoveAbsolute(_) => "move_abs",
        }
    }
}

impl Command {
    /// The frame line of the direct-reply parsing of this command's operation.
    fn direct_parse_line(&self) -> Option<u32> {
        match self {
            Command::Servo(true) => Some(701),
            Command::Servo(false) => Some(715),
            Command::InternalStatusReset => Some(687),
            Command::StatusCheck => Some(605),
            Command::CancelMovement => Some(755),
            Command::Homing(_) => Some(741),
            Command::Velocity { .. } | Command::VelocityHex { .. } => Some(673),
            Command::Read(_) | Command::MoveAbsolute(_) => None,
        }
    }

    /// [`method_name`](Self::method_name) as an error's frame names it.
    fn qualified_method(&self) -> &'static str {
        match self {
            Command::Servo(true) => "TMBSCOMProtocol.set_son",
            Command::Servo(false) => "TMBSCOMProtocol.set_soff",
            Command::InternalStatusReset => "TMBSCOMProtocol.check_alrm",
            Command::StatusCheck => "TMBSCOMProtocol.reset_alarm",
            Command::CancelMovement => "TMBSCOMProtocol.cancel_movement",
            Command::Homing(_) => "TMBSCOMProtocol.move_org",
            Command::Velocity { .. } | Command::VelocityHex { .. } => "TMBSCOMProtocol.write_velocity",
            Command::MoveAbsolute(_) => "TMBSCOMProtocol.move_abs",
            Command::Read(register) => register.qualified_method(),
        }
    }
}

impl Register {
    /// The frames of a reply this register's read cannot convert: the alarm read first hands the text to
    /// the alarm parser, where the conversion fails; the other reads convert it themselves.
    fn raised(self, error: ParseError) -> ParseError {
        match self {
            Register::Alarm => error
                .within(method_frame("TMBSCOMProtocol.parseALRM", 297))
                .within(method_frame("TMBSCOMProtocol.readALRMflags", 595)),
            Register::SerialNumber => error.within(method_frame("TMBSCOMProtocol.readSerialNumber", 551)),
            Register::HomingPattern => error.within(method_frame("TMBSCOMProtocol.readHomingPattern", 541)),
            Register::ModelCode => error.within(method_frame("TMBSCOMProtocol.readModelCode", 561)),
            Register::Status => error.within(method_frame("TMBSCOMProtocol.readSTATflags", 571)),
        }
    }

    fn qualified_method(self) -> &'static str {
        match self {
            Register::ModelCode => "TMBSCOMProtocol.readModelCode",
            Register::SerialNumber => "TMBSCOMProtocol.readSerialNumber",
            Register::HomingPattern => "TMBSCOMProtocol.readHomingPattern",
            Register::Status => "TMBSCOMProtocol.readSTATflags",
            Register::Alarm => "TMBSCOMProtocol.readALRMflags",
        }
    }

    /// The name of the operation that reads this register.
    pub fn method_name(self) -> &'static str {
        match self {
            Register::ModelCode => "readModelCode",
            Register::SerialNumber => "readSerialNumber",
            Register::HomingPattern => "readHomingPattern",
            Register::Status => "readSTATflags",
            Register::Alarm => "readALRMflags",
        }
    }
}

/// STAT register flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatFlags {
    pub main_power_on: bool,
    pub servo_on: bool,
    pub running: bool,
    pub homing_complete: bool,
    pub buffering_hold: bool,
    pub plus_interlock: bool,
    pub minus_interlock: bool,
    pub command_rejected: bool,
}

impl StatFlags {
    pub fn from_bits(v: i64) -> Self {
        let bit = |b: i64| v & b != 0;
        StatFlags {
            main_power_on: bit(0x01),
            servo_on: bit(0x02),
            running: bit(0x04),
            homing_complete: bit(0x08),
            buffering_hold: bit(0x10),
            plus_interlock: bit(0x20),
            minus_interlock: bit(0x40),
            command_rejected: bit(0x80),
        }
    }
}

/// Text of an alarm code.
pub fn alarm_text(code: i64) -> &'static str {
    match code {
        0x00 => "Normal operation",
        0x5A => "Receipt signal over run error",
        0x5B => "Receipt signal flaming error",
        0x5D => "Abnormal header of bucket receipt signal",
        0x5E => "Abnormal delimiter of bucket receipt signal",
        0x7F => "Block check character fault (BCC)",
        0x61 => "Illegal function character, or illegal write address",
        0x62..=0x64 => "Operand illegal in bucket",
        0x70 => "Movement order in RUN condition bit OFF status",
        0x71 => "PTP movement order of absolute position coordinate for ZFIN signal OFF status",
        0x73 => "Alarm reset in case of servo ON order status bit ON",
        0x74 => "Movement order during motor energizing signal detect operation",
        0x75 => "Q2, Q3, m, v, l, j command orders during homing",
        0xB0 => "Data abnormal bank 30",
        0xB1 => "Data abnormal bank 31",
        0xB8 | 0xB9 => "Motor energizing phase detect operation abnormal",
        0xBB..=0xBE => "Encoder signal detect defective during homing, Homing movement abnormal",
        0xC0 | 0xC1 => "Over actual speed, servo abnormal",
        0xC8 => "Over current",
        0xD0 | 0xD1 => "Main power input voltage over value",
        0xD8 => "Over position deviation",
        0xE0 => "Over load",
        0xE8..=0xEC => "Encoder wire disconnected",
        0xED | 0xEE => "Absolute encoder abnormal detection",
        0xF8 => "EEPROM memory data broken",
        _ => "Undefined alarm",
    }
}

/// The exact bytes consumed by the failed integer conversion, distinct from a valid wide integer
/// that does not fit an `i64`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    input: Vec<u8>,
    kind: python::HexParseError,
    /// The frames the error passes on its way out, outermost first.
    frames: Vec<PyFrame>,
}

/// The file name in the frames of this protocol's errors.
pub(crate) const PYTBMSCOM_FILE: &str = "backend/controllers/pytbmscom.py";

/// The frame of the 32-bit packing of an absolute move's position.
pub(crate) const MOVE_ABS_INTEGER_PACK: PyFrame = PyFrame::new(PYTBMSCOM_FILE, "TMBSCOMProtocol.move_abs_integer", 625);

/// A frame of one of this protocol's operations.
const fn method_frame(function: &'static str, line: u32) -> PyFrame {
    PyFrame::new(PYTBMSCOM_FILE, function, line)
}

/// The error of opening the protocol at `caller` when its port does not
/// open, with the frames it passes before the caller handles it: the protocol's set-up, then the
/// serial port's set-up and open. The message is the platform's.
pub(crate) fn open_exception(error: crate::serial::LinkError, caller: PyFrame) -> crate::motion::MotionError {
    use crate::motion::{MotionError, PYSERIAL_BASE_FILE, PYSERIAL_PORT_FILE};
    MotionError::Link(error).in_frames([
        caller,
        method_frame("TMBSCOMProtocol.__init__", 18),
        PyFrame::new(PYSERIAL_PORT_FILE, "__init__", 33),
        PyFrame::new(PYSERIAL_BASE_FILE, "__init__", 244),
        PyFrame::new(PYSERIAL_PORT_FILE, "open", 64),
    ])
}

impl ParseError {
    pub fn input(&self) -> &[u8] {
        &self.input
    }

    /// The frames of the error: the operation the driver called, at the line of the failing
    /// conversion or of the call into the helper that holds it, then that helper.
    pub fn frames(&self) -> &[PyFrame] {
        &self.frames
    }

    fn within(mut self, outer: PyFrame) -> Self {
        self.frames.insert(0, outer);
        self
    }

    pub fn is_value_error(&self) -> bool {
        self.kind == python::HexParseError::InvalidLiteral
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The message quotes at most 200 characters of the input.
        let repr: String = python::bytes_repr(&self.input).chars().take(200).collect();
        if self.is_value_error() {
            write!(formatter, "invalid literal for int() with base 16: {repr}")
        } else {
            write!(formatter, "TMBS hexadecimal integer exceeds the current i64 projection: {repr}")
        }
    }
}

impl std::error::Error for ParseError {}

fn parsed_hex(input: &[u8]) -> Result<i64, ParseError> {
    python::parse_hex_bytes(input).map_err(|kind| ParseError { input: input.to_vec(), kind, frames: Vec::new() })
}

/// The four bytes of a direct response: STAT, ALRM, PI, PO.
///
/// Fewer than 8 characters yield zeros; otherwise four hex pairs after
/// the 2-character prefix.
pub fn parse_direct(data: &[u8]) -> Result<[i64; 4], ParseError> {
    if data.len() < 8 {
        python_print(&["parseADirectResponseFormat - failed to parse message"]);
        return Ok([0; 4]);
    }
    // The four hexadecimal conversions (STAT, ALRM, PI, PO).
    let field = |i: usize| {
        let start = 2 + 2 * i;
        let slice = data.get(start..(start + 2).min(data.len())).unwrap_or(&[]);
        parsed_hex(slice).map_err(|error| {
            error.within(method_frame("TMBSCOMProtocol.parseADirectResponseFormat", 217 + 2 * i as u32))
        })
    };
    Ok([field(0)?, field(1)?, field(2)?, field(3)?])
}

/// One TMBS controller on a serial link. Replies are read from a stream: bytes left
/// unread by one exchange are seen by the next.
pub struct TmbsLink {
    link: Box<dyn Link>,
    rx: VecDeque<u8>,
}

impl TmbsLink {
    pub fn new(link: Box<dyn Link>) -> Self {
        TmbsLink { link, rx: VecDeque::new() }
    }

    pub fn port(&self) -> &str {
        self.link.name()
    }

    /// Where this protocol object's port lives: the address the object's printed form shows.
    pub fn object_address(&self) -> usize {
        std::ptr::from_ref::<dyn Link>(self.link.as_ref()).cast::<()>() as usize
    }

    /// Sends a message: pack and write; a failed write reports `false`.
    pub fn send(&mut self, axis: u8, command: &Command) -> bool {
        self.send_text(u32::from(axis), &command.text())
    }

    pub(crate) fn send_text(&mut self, axis: u32, command: &str) -> bool {
        let frame = pack_axis(axis, command);
        match self.link.write_all(&frame) {
            Ok(()) => true,
            Err(e) => {
                python_print(&["sendMessage - failed sock_serial.write: ", &python::link_error_str(&e)]);
                false
            }
        }
    }

    /// Reads a response: skip to `STX` (a failure there is handled on its own), then read through `ETX` and unpack.
    pub fn read_response(&mut self, clock: &mut dyn Clock) -> Option<Vec<u8>> {
        self.read_response_with_print(clock, python_print)
    }

    fn read_response_with_print(&mut self, clock: &mut dyn Clock, mut print: impl FnMut(&[&str])) -> Option<Vec<u8>> {
        // The two steps fail separately: after a failed skip
        // the body read is still attempted; a failed body gives no response.
        let _ = self.read_until(STX, clock);
        let mut message = vec![STX];
        message.extend(self.read_until(ETX, clock)?);
        match unpack(&message) {
            Ok(payload) => Some(payload),
            Err(UnpackError::TooShort(len)) => {
                print(&[
                    "unpackMessage - message size less than expected, looks like its using the wrong baud rate",
                    &len.to_string(),
                    &python::bytes_repr(&message),
                ]);
                None
            }
            Err(UnpackError::BadCheck) => {
                print(&["unpackMessage - bcc wrong"]);
                None
            }
            Err(UnpackError::NotText(error)) => {
                print(&[&error.caught_traceback()]);
                None
            }
        }
    }

    /// Reads up to a delimiter with the 0.5 s timeout: the bytes up to and including the delimiter,
    /// or whatever arrived before the timeout, read in the system read that waits for them (the 0.5 s are one
    /// limit for the whole call). `None` on a read error.
    fn read_until(&mut self, delimiter: u8, clock: &mut dyn Clock) -> Option<Vec<u8>> {
        let start = clock.now();
        let mut out = Vec::new();
        loop {
            while let Some(b) = self.rx.pop_front() {
                out.push(b);
                if b == delimiter {
                    return Some(out);
                }
            }
            let Some(remaining) = READ_TIMEOUT.checked_sub(clock.now().saturating_sub(start)).filter(|r| !r.is_zero())
            else {
                return Some(out);
            };
            let before = clock.now();
            match self.link.read_blocking(remaining) {
                Ok(bytes) if bytes.is_empty() => {
                    // The read waited out the rest of the timeout (a link without a blocking read returns at once:
                    // the wait is counted on the clock).
                    if clock.now() == before {
                        clock.sleep(remaining);
                    }
                    return Some(out);
                }
                Ok(bytes) => self.rx.extend(bytes),
                Err(e) => {
                    debug!(port = self.link.name(), "readResponse - {e}");
                    return None;
                }
            }
        }
    }

    /// Send a command and read its reply when the write succeeded.
    fn exchange(&mut self, axis: u8, command: &Command, clock: &mut dyn Clock) -> Option<Vec<u8>> {
        if !self.send(axis, command) {
            return None;
        }
        self.read_response(clock)
    }

    /// The hexadecimal value of a register read (the payload after its two-character prefix), 0 when the
    /// controller did not answer.
    ///
    /// Used for the model code, the serial number, the homing pattern and the ALRM flags.
    pub fn read_register(&mut self, axis: u8, register: Register, clock: &mut dyn Clock) -> Result<i64, ParseError> {
        let Some(payload) = self.exchange(axis, &Command::Read(register), clock) else {
            python_print(&[&format!("{} - failed", register.method_name())]);
            return Ok(0);
        };
        let value = parsed_hex(payload.get(2..).unwrap_or(&[])).map_err(|error| register.raised(error))?;
        if register == Register::Alarm {
            // The alarm text is printed.
            python_print(&[alarm_text(value)]);
        }
        Ok(value)
    }

    /// The STAT flags, or `None` when the controller did not answer.
    pub fn read_status(&mut self, axis: u8, clock: &mut dyn Clock) -> Result<Option<StatFlags>, ParseError> {
        let Some(payload) = self.exchange(axis, &Command::Read(Register::Status), clock) else {
            python_print(&["readSTATflags - failed"]);
            return Ok(None);
        };
        let value = parsed_hex(payload.get(2..).unwrap_or(&[])).map_err(|error| Register::Status.raised(error))?;
        Ok(Some(StatFlags::from_bits(value)))
    }

    /// Send a command and parse its direct reply; `false` when not sent or not answered.
    ///
    /// Used for servo on and off, the alarm check and reset, the movement cancellation, homing and
    /// the velocity write (each parses STAT/ALRM/PI/PO only for its debug output).
    pub fn action(&mut self, axis: u8, command: &Command, clock: &mut dyn Clock) -> Result<bool, ParseError> {
        self.action_axis(u32::from(axis), command, clock)
    }

    pub(crate) fn action_axis(
        &mut self,
        axis: u32,
        command: &Command,
        clock: &mut dyn Clock,
    ) -> Result<bool, ParseError> {
        let payload = if self.send_text(axis, &command.text()) { self.read_response(clock) } else { None };
        let Some(payload) = payload else {
            python_print(&[&format!("{} - failed", command.method_name())]);
            return Ok(false);
        };
        // The direct reply is parsed in the frame of the operation that sent the command.
        parse_direct(&payload).map_err(|error| match command.direct_parse_line() {
            Some(line) => error.within(PyFrame::new(PYTBMSCOM_FILE, command.qualified_method(), line)),
            None => error,
        })?;
        Ok(true)
    }

    /// Absolute move without reading the reply.
    pub fn move_absolute(&mut self, axis: u8, position: i32) -> bool {
        self.send(axis, &Command::MoveAbsolute(position))
    }
}

/// 8 upper-case hex digits of the low 32 bits: the serial number text of the Dyadic drivers.
pub fn convert_sn(sn: i64) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    (0..8).rev().map(|i| char::from(DIGITS[((sn >> (4 * i)) & 15) as usize])).collect()
}

#[cfg(test)]
pub(crate) mod testing {
    //! A scripted TMBS controller answering by register/command.

    use super::*;

    /// The reply frame a controller sends for `payload` on `axis`.
    pub fn reply(axis: char, payload: &str) -> Vec<u8> {
        let body = format!("U{axis}{payload}");
        let mut out = vec![STX];
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(&bcc(body.as_bytes()));
        out.push(ETX);
        out
    }

    /// The request bytes `pack(0, command)`.
    pub fn request(command: &Command) -> Vec<u8> {
        pack(0, &command.text())
    }
}

#[cfg(test)]
mod tests {
    use super::testing::reply;
    use super::*;
    use crate::motion::driver_support::testing::ScriptedLink;

    #[derive(Default)]
    struct StepClock(Duration);
    impl Clock for StepClock {
        fn now(&self) -> Duration {
            self.0
        }
        fn sleep(&mut self, d: Duration) {
            self.0 += d;
        }
    }

    #[test]
    fn calc_bcc_is_twos_complement_of_byte_sum() {
        // The check code: (((sum & 0xFF) ^ 0xFF) + 1) & 0xFF, upper hex.
        assert_eq!(&bcc(b"0aFFFFEC7800"), b"00");
        assert_eq!(&bcc(b"0R4000068000"), b"8C");
        assert_eq!(&bcc(b""), b"00");
    }

    #[test]
    fn pack_message_example_from_move_abs_integer() {
        // An absolute move of axis 0 to -5000 → 02 30 'aFFFFEC78' '00' '00' 03.
        let f = pack(0, &Command::MoveAbsolute(-5000).text());
        assert_eq!(f, b"\x020aFFFFEC780000\x03");
        assert_eq!(f.len(), 16);
    }

    #[test]
    fn command_strings() {
        assert_eq!(Command::Read(Register::Status).text(), "R4000074030");
        assert_eq!(Command::Servo(true).text(), "q1000000000");
        assert_eq!(Command::Servo(false).text(), "q0000000000");
        assert_eq!(Command::InternalStatusReset.text(), "r0300000000");
        assert_eq!(Command::StatusCheck.text(), "n0000000000");
        assert_eq!(Command::CancelMovement.text(), "d0000000000");
        assert_eq!(Command::Homing("07".into()).text(), "o0700000000");
        // The velocity write: 'v' + '2' + 5000 and 80 as four hex digits each + '0'.
        assert_eq!(Command::Velocity { speed: 5000, accel: 80 }.text(), "v2138800500");
        assert_eq!(Command::MoveAbsolute(10000).text(), "a0000271000");
    }

    #[test]
    fn percent_04x_matches_python() {
        assert_eq!(py_hex4(80), "0050");
        assert_eq!(py_hex4(0xABCDE), "abcde");
        assert_eq!(py_hex4(-1), "-001");
        assert_eq!(py_hex4(-0x1234), "-1234");
    }

    #[test]
    fn int_base16_matches_python() {
        assert_eq!(py_int_hex(b"0000000B0"), Some(0xB0));
        assert_eq!(py_int_hex(b" 1f "), Some(0x1F));
        assert_eq!(py_int_hex(b"0x1F"), Some(0x1F));
        assert_eq!(py_int_hex(b"-a"), Some(-10));
        assert_eq!(py_int_hex(b"1_0"), Some(16));
        assert_eq!(py_int_hex(b""), None);
        assert_eq!(py_int_hex(b"OK"), None);
        assert_eq!(py_int_hex(b"1__0"), None);
    }

    #[test]
    fn unpack_message_checks_length_and_bcc() {
        let good = reply('0', "R4808976450");
        assert_eq!(good.len(), 17);
        assert_eq!(unpack(&good).unwrap(), b"R4808976450");
        let mut bad = good.clone();
        bad[5] = b'9';
        assert_eq!(unpack(&bad), Err(UnpackError::BadCheck));
        assert_eq!(unpack(b"\x02U0R4\x03"), Err(UnpackError::TooShort(6)));
    }

    fn raw_reply(axis: u8, payload: &[u8], check: &[u8; 2]) -> Vec<u8> {
        let mut message = vec![STX, b'U', axis];
        message.extend_from_slice(payload);
        message.extend_from_slice(check);
        message.push(ETX);
        message
    }

    #[test]
    fn native_tmbs_utf8_decode_errors_keep_payload_relative_spans_and_reasons() {
        let cases = [
            (&b"R4000000\xff0"[..], "'utf-8' codec can't decode byte 0xff in position 8: invalid start byte"),
            (
                &b"R4000000\xe2(\xa1"[..],
                "'utf-8' codec can't decode byte 0xe2 in position 8: invalid continuation byte",
            ),
            (&b"R4000000\xe2\x82("[..], "'utf-8' codec can't decode bytes in position 8-9: invalid continuation byte"),
            (&b"R40000000\xe2"[..], "'utf-8' codec can't decode byte 0xe2 in position 9: unexpected end of data"),
            (&b"R4000000\xe2\x82"[..], "'utf-8' codec can't decode bytes in position 8-9: unexpected end of data"),
            (
                &b"R4000000\xf5\x80\x80\x80"[..],
                "'utf-8' codec can't decode byte 0xf5 in position 8: invalid start byte",
            ),
            (
                &b"R4000000\xed\xa0\x80"[..],
                "'utf-8' codec can't decode byte 0xed in position 8: invalid continuation byte",
            ),
            (
                &b"R4000000\xe0\x80\x80"[..],
                "'utf-8' codec can't decode byte 0xe0 in position 8: invalid continuation byte",
            ),
            (
                &b"R4000000\xf4\x90\x80\x80"[..],
                "'utf-8' codec can't decode byte 0xf4 in position 8: invalid continuation byte",
            ),
            (&b"R4000000\x800"[..], "'utf-8' codec can't decode byte 0x80 in position 8: invalid start byte"),
            (
                &b"R4000000\xf0\x90\x80("[..],
                "'utf-8' codec can't decode bytes in position 8-10: invalid continuation byte",
            ),
        ];
        let spans = [(8, 9), (8, 9), (8, 10), (9, 10), (8, 10), (8, 9), (8, 9), (8, 9), (8, 9), (8, 9), (8, 11)];
        for ((payload, expected), (start, end)) in cases.into_iter().zip(spans) {
            let error = unpack(&raw_reply(b'0', payload, b"00")).unwrap_err();
            assert_eq!(error.to_string(), expected);
            let UnpackError::NotText(error) = error else { panic!("payload decode must precede the check") };
            assert_eq!(error.payload, payload);
            assert_eq!((error.start, error.end), (start, end));
            assert_eq!(error.reason, expected.rsplit_once(": ").unwrap().1);
        }
    }

    #[test]
    fn native_tmbs_utf8_size_decode_and_checksum_fail_in_original_order() {
        let short = raw_reply(b'0', b"R4\xff", b"00");
        assert_eq!(unpack(&short), Err(UnpackError::TooShort(9)));
        assert!(matches!(unpack(&raw_reply(b'0', b"R4000000\xff0", b"00")), Err(UnpackError::NotText(_))));
        assert_eq!(unpack(&raw_reply(b'0', b"R4808976450", b"00")), Err(UnpackError::BadCheck));
    }

    #[test]
    fn native_tmbs_utf8_unicode_bytes_and_high_axis_keep_the_source_checksum() {
        // The check code sums the UTF-8 bytes of the text; the axis character is encoded too.
        let payload = "R4é000000".as_bytes();
        assert_eq!(pack(0, "R4é000000"), b"\x020R4\xc3\xa9000000BE\x03");
        assert_eq!(bcc("UÿR4é000000".as_bytes()), *b"17");
        assert_eq!(unpack(&raw_reply(0xff, payload, b"17")), Ok(payload.to_vec()));
        assert_eq!(
            unpack(&raw_reply(0xff, payload, b"9A")),
            Err(UnpackError::BadCheck),
            "the raw-axis-byte sum differs"
        );
        let mut frame = raw_reply(0xff, payload, b"17");
        frame[0] = b'!';
        frame[1] = b'?';
        *frame.last_mut().unwrap() = b'!';
        assert_eq!(unpack(&frame), Ok(payload.to_vec()), "the original slices the markers without validating them");
    }

    #[test]
    fn native_tmbs_utf8_caught_traceback_prints_and_keeps_the_next_frame() {
        let mut frames = raw_reply(b'0', b"R4000000\xff0", b"00");
        frames.extend(reply('0', "R4808976450"));
        let mut link = TmbsLink::new(Box::new(ScriptedLink::new("tmbs", move |_| frames.clone())));
        assert!(link.send(0, &Command::Read(Register::ModelCode)));
        let mut clock = StepClock::default();
        let mut output = String::new();
        assert_eq!(
            link.read_response_with_print(&mut clock, |parts| {
                output.push_str(&parts.join(" "));
                output.push('\n');
            }),
            None
        );
        assert_eq!(output, concat!(
            "Traceback (most recent call last):\n",
            "  File \"backend/controllers/pytbmscom.py\", line 79, in backend.controllers.pytbmscom.TMBSCOMProtocol.readResponse\n",
            "  File \"backend/controllers/pytbmscom.py\", line 119, in backend.controllers.pytbmscom.TMBSCOMProtocol.unpackMessage\n",
            "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position 8: invalid start byte\n\n",
        ));
        assert_eq!(
            link.read_response_with_print(&mut clock, |_| panic!("the following frame is valid")),
            Some(b"R4808976450".to_vec())
        );
    }

    #[test]
    fn native_tmbs_utf8_normal_rejections_keep_the_original_prints() {
        for (frame, expected) in [
            (raw_reply(b'0', b"bad", b"00"), "unpackMessage - message size less than expected, looks like its using the wrong baud rate 9 b'\\x02U0bad00\\x03'\n"),
            (raw_reply(b'0', b"R4808976450", b"00"), "unpackMessage - bcc wrong\n"),
        ] {
            let mut link = TmbsLink::new(Box::new(ScriptedLink::new("tmbs", move |_| frame.clone())));
            assert!(link.send(0, &Command::Read(Register::ModelCode)));
            let mut output = String::new();
            assert_eq!(link.read_response_with_print(&mut StepClock::default(), |parts| {
                output.push_str(&parts.join(" "));
                output.push('\n');
            }), None);
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn direct_response_fields() {
        // The direct reply: characters 2..4, 4..6, 6..8, 8..10; < 8 chars → zeros.
        assert_eq!(parse_direct(b"Q10B00F0A1").unwrap(), [0x0B, 0x00, 0xF0, 0xA1]);
        assert_eq!(parse_direct(b"R40000").unwrap(), [0; 4]);
        // 8 or 9 characters: the last field is empty or a single digit (an empty field is an error).
        assert!(parse_direct(b"R4000000").is_err());
        assert!(parse_direct(b"R4ZZ000000").is_err());
    }

    #[test]
    fn native_tmbs_parse_errors_describe_the_consumed_direct_field_bytes() {
        for payload in [b"R4zz000000", b"R400zz0000", b"R40000zz00", b"R4000000zz"] {
            assert_eq!(parse_direct(payload).unwrap_err().to_string(), "invalid literal for int() with base 16: b'zz'");
        }
        assert_eq!(parse_direct(b"R4000000").unwrap_err().to_string(), "invalid literal for int() with base 16: b''");
        assert_eq!(
            parse_direct(b"R4\xc3\xa4000000").unwrap_err().to_string(),
            "invalid literal for int() with base 16: b'\\xc3\\xa4'"
        );
    }

    #[test]
    fn native_tmbs_register_and_status_errors_exclude_the_protocol_prefix() {
        let link = ScriptedLink::new("tmbs", |_| reply('0', "R4zz000000"));
        let mut link = TmbsLink::new(Box::new(link));
        let mut clock = StepClock::default();
        for register in [Register::ModelCode, Register::SerialNumber, Register::HomingPattern, Register::Alarm] {
            assert_eq!(
                link.read_register(0, register, &mut clock).unwrap_err().to_string(),
                "invalid literal for int() with base 16: b'zz000000'"
            );
        }
        assert_eq!(
            link.read_status(0, &mut clock).unwrap_err().to_string(),
            "invalid literal for int() with base 16: b'zz000000'"
        );
    }

    #[test]
    fn stat_bits() {
        // R40000000B = Main power ON + Servo ON + Homing complete.
        let f = StatFlags::from_bits(0x0B);
        assert!(f.main_power_on && f.servo_on && f.homing_complete);
        assert!(!f.running && !f.command_rejected);
    }

    #[test]
    fn convert_sn_is_eight_upper_hex_digits() {
        assert_eq!(convert_sn(0x1234ABCD), "1234ABCD");
        assert_eq!(convert_sn(0x9_0000_0001), "00000001");
        assert_eq!(convert_sn(7), "00000007");
    }

    #[test]
    fn alarm_codes() {
        assert_eq!(alarm_text(0x7F), "Block check character fault (BCC)");
        assert_eq!(alarm_text(0x63), "Operand illegal in bucket");
        assert_eq!(alarm_text(0x99), "Undefined alarm");
    }

    fn scripted(answer: &'static [(&'static str, &'static str)]) -> TmbsLink {
        let link = ScriptedLink::new("tmbs", move |bytes| {
            let text = std::str::from_utf8(&bytes[2..13]).unwrap();
            answer
                .iter()
                .find(|(cmd, _)| *cmd == text)
                .map(|(_, payload)| {
                    // dirty bytes before the frame are skipped by the read up to STX
                    let mut v = b"\xff\x00".to_vec();
                    v.extend(reply('0', payload));
                    v
                })
                .unwrap_or_default()
        });
        TmbsLink::new(Box::new(link))
    }

    #[test]
    fn register_reads_parse_payload_after_two_chars() {
        // The model code is the hexadecimal payload after its prefix; a homing pattern reply 'R400000007' → 7.
        let mut t =
            scripted(&[("R4000068000", "R4808976450"), ("R4000000060", "R400000007"), ("R4000074040", "OK00000000")]);
        let mut clock = StepClock::default();
        assert_eq!(t.read_register(0, Register::ModelCode, &mut clock).unwrap(), 0x808976450);
        assert_eq!(t.read_register(0, Register::HomingPattern, &mut clock).unwrap(), 7);
        assert_eq!(t.read_register(0, Register::Alarm, &mut clock).unwrap(), 0);
        let mut signed = scripted(&[("R4000068000", "R4-8000000000000000")]);
        assert_eq!(signed.read_register(0, Register::ModelCode, &mut clock).unwrap(), i64::MIN);
    }

    #[test]
    fn unanswered_reads_return_zero_after_timeouts() {
        // The model code read prints 'failed' and returns 0; the read waits 0.5 s for STX and again for ETX.
        let mut t = scripted(&[]);
        let mut clock = StepClock::default();
        assert_eq!(t.read_register(0, Register::ModelCode, &mut clock).unwrap(), 0);
        assert!(clock.0 >= 2 * READ_TIMEOUT);
        assert_eq!(t.read_status(0, &mut clock).unwrap(), None);
    }

    #[test]
    fn shared_callers_keep_separate_skip_and_body_read_errors() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        struct FaultLink {
            steps: VecDeque<Option<Vec<u8>>>,
            reads: Arc<AtomicUsize>,
        }
        impl Link for FaultLink {
            fn write_all(&mut self, _: &[u8]) -> Result<(), crate::serial::LinkError> {
                Ok(())
            }
            fn read_available(&mut self) -> Result<Vec<u8>, crate::serial::LinkError> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                self.steps
                    .pop_front()
                    .unwrap_or(Some(Vec::new()))
                    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "scripted read failure").into())
            }
            fn clear_input(&mut self) -> Result<(), crate::serial::LinkError> {
                Ok(())
            }
            fn name(&self) -> &str {
                "scripted TMBS"
            }
        }
        let response = reply('0', "R4808976450");
        let mut partial = response[1..response.len() - 1].to_vec();
        partial.push(b'!'); // No ETX: the next read fails after bytes that would pass unpacking.
        let cases = [
            (vec![None, Some(response[1..].to_vec())], 0x8_0897_6450, 2),
            (vec![Some(vec![STX]), Some(partial), None], 0, 3),
        ];
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        for simtech in [false, true] {
            for (steps, expected_value, count) in &cases {
                let reads = Arc::new(AtomicUsize::new(0));
                let link = Box::new(FaultLink { steps: steps.clone().into(), reads: reads.clone() });
                let mut clock = StepClock::default();
                let value = if simtech {
                    crate::motion::simtech::TmbsInterface::new(link).read_model_code(&mut clock).unwrap()
                } else {
                    TmbsLink::new(link).read_register(0, Register::ModelCode, &mut clock).unwrap()
                };
                actual.push((value, reads.load(Ordering::SeqCst)));
                expected.push((*expected_value, *count));
            }
        }
        assert_eq!(actual, expected, "SCN6 then SimTech; skipped-read error then body-read error");
    }

    #[test]
    fn unparsable_register_payload_is_an_error() {
        let mut t = scripted(&[("R4000068000", "R4XYZXYZXYZ")]);
        let mut clock = StepClock::default();
        assert!(t.read_register(0, Register::ModelCode, &mut clock).is_err());
    }

    #[test]
    fn actions_report_whether_a_reply_came() {
        let mut t = scripted(&[("q1000000000", "Q10B000000")]);
        let mut clock = StepClock::default();
        assert!(t.action(0, &Command::Servo(true), &mut clock).unwrap());
        assert!(!t.action(0, &Command::Servo(false), &mut clock).unwrap());
    }

    #[test]
    fn stale_replies_are_read_by_the_next_exchange() {
        // An absolute move does not read its reply; the next response read sees it first.
        let mut t = scripted(&[("a0000271000", "A10B000000"), ("R4000074030", "R40000000B")]);
        let mut clock = StepClock::default();
        assert!(t.move_absolute(0, 10000));
        let status = t.read_status(0, &mut clock);
        // The stale 'A10B000000' payload parses as hexadecimal '0B000000' → bits of 0x0B000000: no power flag.
        assert_eq!(status.unwrap(), Some(StatFlags::from_bits(0x0B00_0000)));
    }
}

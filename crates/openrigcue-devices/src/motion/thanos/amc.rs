//! One AMC controller on its serial link: queries, parameter writes, streaming and the XTEA handshake.

use std::time::Duration;

use tracing::debug;

use super::protocol::{self, ConfigError, ConfigKind, ControllerConfig, FieldValue, Parameter, AUTH_RESULT_QUERY};
use super::{at, xtea};
use crate::motion::clock::Clock;
use crate::motion::{python, MotionError, PyFrame, SerialCall};
use crate::serial::{Link, LinkError};

/// The frames of the serial writes of the operations that write a command.
const GET_FIRMWARE_WRITE: PyFrame = at("AMC.get_firmware", 377);
const GET_LONG_CONFIG_WRITE: PyFrame = at("AMC.get_long_config", 385);
const GET_SHORT_CONFIG_WRITE: PyFrame = at("AMC.get_short_config", 394);
const DISPLAY_PARAMETER_WRITE: PyFrame = at("AMC.display_parameter", 410);
const SAVE_PARAMETER_WRITE: PyFrame = at("AMC.save_parameter_value", 367);
const PARK_WRITE: PyFrame = at("AMC.cmd_park_all_actuators_if_in_standby_mode", 684);
/// The frames of the parameter save inside every parameter command and inside
/// the two save-all commands.
const CMD_SAVE_CALL: PyFrame = at("AMC.cmd_read_or_write", 421);
const SAVE_ALL_CALL: PyFrame = at("AMC.cmd_save_all_parameters_at_once", 673);
const SAVE_ALL_SRS_CALL: PyFrame = at("AMC.cmd_save_all_SRS_parameters_at_once", 679);
/// The frame name of the handshake: its three writes and the decryption of the answer.
const VERIFICATION: &str = "AMC.verification_with_encryption";
/// The frames of the block decryption: the data is split into blocks first, and
/// data that is not a whole number of blocks is a value error.
const PEP272_DECRYPT: PyFrame = PyFrame::new("pep272_encryption\\__init__.py", "decrypt", 318);
const PEP272_SPLIT_BLOCKS: PyFrame = PyFrame::new("pep272_encryption\\util.py", "split_blocks", 141);
/// The frames of the reply decoding of the operations that decode a reply.
const GET_FIRMWARE_DECODE: PyFrame = at("AMC.get_firmware", 379);
const GET_LONG_CONFIG_DECODE: PyFrame = at("AMC.get_long_config", 387);
const GET_SHORT_CONFIG_DECODE: PyFrame = at("AMC.get_short_config", 396);
const DISPLAY_PARAMETER_DECODE: PyFrame = at("AMC.display_parameter", 412);
const SAVE_ALL_DECODE: PyFrame = at("AMC.cmd_save_all_parameters_at_once", 674);
const SAVE_ALL_SRS_DECODE: PyFrame = at("AMC.cmd_save_all_SRS_parameters_at_once", 680);
/// The frame name and lines of the configuration read: the short and long queries, the layout test
/// on the fifth field, and the three places a field is stored (as an integer, as text after a failed
/// integer conversion, and as a leading text field).
const GEN_CONFIG_DICT: &str = "AMC.gen_config_dict";
const GEN_CONFIG_SHORT_QUERY: u32 = 224;
const GEN_CONFIG_LAYOUT_TEST: u32 = 226;
const GEN_CONFIG_LONG_QUERY: u32 = 234;
const GEN_CONFIG_INT_VALUE: u32 = 250;
const GEN_CONFIG_EXCEPT_VALUE: u32 = 252;
const GEN_CONFIG_TEXT_VALUE: u32 = 254;
/// The frame of the hexadecimal decoding of the handshake answer.
const VERIFICATION_FROMHEX: PyFrame = at("AMC.verification_with_encryption", 200);

fn index_error() -> MotionError {
    MotionError::Native("IndexError: list index out of range".into())
}

/// The pause after most writes.
const REPLY_PAUSE: Duration = Duration::from_millis(100);
/// The pause after a saved parameter on an `AMC-MDBOX`.
const MDBOX_SAVE_PAUSE: Duration = Duration::from_millis(300);
/// The pause before reading the encrypted answer.
const AUTH_RESULT_PAUSE: Duration = Duration::from_millis(1100);
/// Poll interval of a fixed-size read. The read has no timeout: it waits until its bytes
/// have arrived (or the port fails), however long that takes.
const EXACT_READ_POLL: Duration = Duration::from_millis(5);

/// Key that encrypts the challenge sent to the controller.
pub(super) const REQUEST_KEY: [u8; 16] = *b"318Th4SRS9x6v2G7";
/// Key of the controller's encrypted answer.
pub(super) const ANSWER_KEY: [u8; 16] = *b"Nlzo5?O+^n~\\pT=*";
/// Characters of the random challenge (ASCII lower-case and upper-case letters, then digits).
pub const CHALLENGE_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
/// Length of the challenge.
pub const CHALLENGE_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum AmcError {
    #[error(transparent)]
    Link(#[from] LinkError),
    /// An error of a controller operation, with the frames it passes.
    #[error(transparent)]
    Raised(MotionError),
    #[error("{0}")]
    Protocol(String),
}

impl AmcError {
    /// The error after it passed `frame` (when its frames are known).
    fn through(self, frame: PyFrame) -> AmcError {
        match self {
            AmcError::Raised(error) => AmcError::Raised(error.through(frame)),
            other => other,
        }
    }
}

/// The index error of the configuration read for a reply of `found` fields that is shorter than its layout.
fn config_error(kind: ConfigKind, ConfigError::TooShort { found, .. }: ConfigError) -> AmcError {
    let raised_at = |line| index_error().in_frame(at(GEN_CONFIG_DICT, line));
    let text_fields = match kind {
        ConfigKind::Short => 5,
        ConfigKind::Long => 3,
    };
    AmcError::Raised(if found < text_fields {
        // The short reply's layout test reads the fifth field before the loop starts.
        raised_at(if kind == ConfigKind::Short { GEN_CONFIG_LAYOUT_TEST } else { GEN_CONFIG_TEXT_VALUE })
    } else {
        raised_at(GEN_CONFIG_EXCEPT_VALUE).while_handling(raised_at(GEN_CONFIG_INT_VALUE))
    })
}

/// An AMC controller behind a byte link.
pub struct Amc {
    link: Box<dyn Link>,
    /// Bytes received but not yet consumed by a fixed-size read.
    inbox: Vec<u8>,
}

impl Amc {
    pub fn new(link: Box<dyn Link>) -> Self {
        Amc { link, inbox: Vec::new() }
    }

    pub fn port(&self) -> &str {
        self.link.name()
    }

    /// Reads whatever is waiting.
    fn read_waiting(&mut self) -> Result<Vec<u8>, AmcError> {
        let mut data = std::mem::take(&mut self.inbox);
        data.extend(self.link.read_available()?);
        Ok(data)
    }

    /// Reads exactly `n` bytes without a timeout: blocks until `n` bytes have arrived; only a
    /// failing port ends the wait early.
    fn read_exact(&mut self, n: usize, clock: &mut dyn Clock) -> Result<Vec<u8>, AmcError> {
        loop {
            self.inbox.extend(self.link.read_available()?);
            if self.inbox.len() >= n {
                let rest = self.inbox.split_off(n);
                return Ok(std::mem::replace(&mut self.inbox, rest));
            }
            clock.sleep(EXACT_READ_POLL);
        }
    }

    /// Writes `bytes` at the frame `site`. A write that times out (the port has a
    /// write timeout of 3 s) or meets a closed port carries its frames; a system error
    /// keeps no frames.
    fn write(&mut self, bytes: &[u8], site: PyFrame) -> Result<(), AmcError> {
        self.link.write_all(bytes).map_err(|e| match MotionError::Link(e).in_serial_call(SerialCall::Write, site) {
            MotionError::Link(e) => AmcError::Link(e),
            raised => AmcError::Raised(raised),
        })
    }

    /// Decodes a reply at `frame`: a decode error for a reply that is not UTF-8.
    fn decode(bytes: Vec<u8>, frame: PyFrame) -> Result<String, AmcError> {
        String::from_utf8(bytes).map_err(|e| {
            let exception = python::utf8_decode_error(e.as_bytes(), &e.utf8_error());
            AmcError::Raised(MotionError::Native(exception).in_frame(frame))
        })
    }

    /// Writes `query` (at the frame `write`), waits 0.1 s and returns the first `\r\n`-separated line of the
    /// reply, decoded at the frame `decode`. Shared shape of the firmware query and the short and
    /// long configuration queries.
    fn query_first_line(
        &mut self,
        query: &[u8],
        write: PyFrame,
        decode: PyFrame,
        clock: &mut dyn Clock,
    ) -> Result<String, AmcError> {
        self.write(query, write)?;
        clock.sleep(REPLY_PAUSE);
        let reply = Self::decode(self.read_waiting()?, decode)?;
        Ok(reply.split("\r\n").next().unwrap_or_default().to_string())
    }

    /// The firmware text: the first line of the `RQM` reply.
    pub fn firmware(&mut self, clock: &mut dyn Clock) -> Result<String, AmcError> {
        self.query_first_line(protocol::FIRMWARE_QUERY, GET_FIRMWARE_WRITE, GET_FIRMWARE_DECODE, clock)
    }

    /// Reads the short or long configuration of the controller.
    pub fn config(&mut self, kind: ConfigKind, clock: &mut dyn Clock) -> Result<ControllerConfig, AmcError> {
        let (query, write, decode, call_line) = match kind {
            ConfigKind::Short => {
                (protocol::SHORT_CONFIG_QUERY, GET_SHORT_CONFIG_WRITE, GET_SHORT_CONFIG_DECODE, GEN_CONFIG_SHORT_QUERY)
            }
            ConfigKind::Long => {
                (protocol::LONG_CONFIG_QUERY, GET_LONG_CONFIG_WRITE, GET_LONG_CONFIG_DECODE, GEN_CONFIG_LONG_QUERY)
            }
        };
        let line = self
            .query_first_line(query, write, decode, clock)
            .map_err(|e| e.through(at(GEN_CONFIG_DICT, call_line)))?;
        debug!("raw config is {line}");
        protocol::parse_config(kind, &line).map_err(|e| config_error(kind, e))
    }

    /// Reads the parameter `code` (the read path of every parameter command).
    pub fn read_parameter(&mut self, code: &str, clock: &mut dyn Clock) -> Result<String, AmcError> {
        self.write(&protocol::read_parameter_frame(code), DISPLAY_PARAMETER_WRITE)?;
        clock.sleep(REPLY_PAUSE);
        Self::decode(self.read_waiting()?, DISPLAY_PARAMETER_DECODE)
    }

    /// Saves a parameter value: `spv<code><value>`, then 0.3 s on an MDBOX, else 0.1 s.
    fn save_raw(&mut self, code: &str, value: &str, mdbox: bool, clock: &mut dyn Clock) -> Result<(), AmcError> {
        self.write(&protocol::save_parameter_frame(code, value), SAVE_PARAMETER_WRITE)?;
        clock.sleep(if mdbox { MDBOX_SAVE_PAUSE } else { REPLY_PAUSE });
        Ok(())
    }

    /// The write path of the parameter commands. Fails where the value cannot be formatted.
    pub fn write_parameter(
        &mut self,
        parameter: Parameter,
        value: &FieldValue,
        mdbox: bool,
        clock: &mut dyn Clock,
    ) -> Result<(), AmcError> {
        if parameter.motor().is_some_and(|m| m > 3) {
            crate::motion::python_print(&["motor number must be between 0 and 3"]);
        }
        // A text value cannot be formatted as a two- or three-digit number.
        let text = parameter.format_value(value).ok_or_else(|| {
            let raised = MotionError::Native("TypeError: %d format: a real number is required, not str".into());
            AmcError::Raised(match parameter.format_site() {
                Some((method, line)) => raised.in_frame(at(method, line)),
                None => raised,
            })
        })?;
        // The frames: the parameter command, its read-or-write step, the save.
        let (method, call_line) = parameter.command_site();
        self.save_raw(&parameter.code(), &text, mdbox, clock)
            .map_err(|e| e.through(CMD_SAVE_CALL).through(at(method, call_line)))
    }

    /// [`write_parameter`](Self::write_parameter) of an integer of any width (the spike-filter commands of
    /// a motion frame).
    pub fn write_parameter_wide(
        &mut self,
        parameter: Parameter,
        value: &num_bigint::BigInt,
        mdbox: bool,
        clock: &mut dyn Clock,
    ) -> Result<(), AmcError> {
        let text = parameter.format_wide(value);
        let (method, call_line) = parameter.command_site();
        self.save_raw(&parameter.code(), &text, mdbox, clock)
            .map_err(|e| e.through(CMD_SAVE_CALL).through(at(method, call_line)))
    }

    /// Saves all parameters at once (`srs == false`, `spv45`) or
    /// all SRS parameters at once (`srs == true`, `spv46`); returns the controller's reply.
    pub fn save_all(&mut self, srs: bool, mdbox: bool, clock: &mut dyn Clock) -> Result<String, AmcError> {
        let code = if srs { protocol::SAVE_ALL_SRS_CODE } else { protocol::SAVE_ALL_CODE };
        self.save_raw(code, "", mdbox, clock)
            .map_err(|e| e.through(if srs { SAVE_ALL_SRS_CALL } else { SAVE_ALL_CALL }))?;
        Self::decode(self.read_waiting()?, if srs { SAVE_ALL_SRS_DECODE } else { SAVE_ALL_DECODE })
    }

    /// Parks all actuators if the controller is in standby: the raw bytes `Park`, no terminator.
    pub fn park_if_standby(&mut self) -> Result<(), AmcError> {
        self.write(protocol::PARK_IF_STANDBY, PARK_WRITE)
    }

    /// Sends one position frame.
    pub fn send_positions(&mut self, positions: &[i64; 8]) -> Result<(), AmcError> {
        self.link.write_all(&protocol::position_frame(positions))?;
        Ok(())
    }

    /// Sends one position frame of positions already reduced to 16 bits.
    pub fn send_words(&mut self, words: &[u16]) -> Result<(), AmcError> {
        self.link.write_all(&protocol::position_frame_words(words))?;
        Ok(())
    }

    /// The XTEA handshake with the given 16-character challenge.
    /// `Ok(false)` for a controller that does not answer as expected; `Err` for an answer that cannot be decoded.
    pub fn authenticate(&mut self, challenge: &str, clock: &mut dyn Clock) -> Result<bool, AmcError> {
        let encrypted = xtea::encrypt_ecb(&REQUEST_KEY, challenge.as_bytes())
            .ok_or_else(|| AmcError::Protocol("challenge is not a whole number of blocks".into()))?;
        let hex = upper_hex(&encrypted);
        let (first, second) = hex.split_at(hex.len().min(16));

        self.write(&protocol::terminated(format!("ZP{first}0").as_bytes()), at(VERIFICATION, 161))?;
        clock.sleep(REPLY_PAUSE);
        let reply = self.read_exact(25, clock)?;
        if !echoes(&reply, b"1st half:", first) {
            return Ok(false);
        }
        clock.sleep(REPLY_PAUSE);
        self.write(&protocol::terminated(format!("ZP{second}1").as_bytes()), at(VERIFICATION, 173))?;
        clock.sleep(REPLY_PAUSE);
        let reply = self.read_exact(27, clock)?;
        clock.sleep(REPLY_PAUSE);
        if !echoes(&reply, b"\r\n2st half:", second) {
            return Ok(false);
        }
        self.write(AUTH_RESULT_QUERY, at(VERIFICATION, 184))?;
        clock.sleep(AUTH_RESULT_PAUSE);
        let Some(fields) = auth_answer_fields(self.read_waiting()?) else {
            return Ok(false);
        };
        if fields.first().map(String::as_str) != Some("TX") {
            return Ok(false);
        }
        // The second field is the hexadecimal cipher text.
        let cipher_hex = fields.get(1).ok_or_else(|| AmcError::Raised(index_error().in_frame(VERIFICATION_FROMHEX)))?;
        let cipher = python::bytes_fromhex(cipher_hex).map_err(|position| {
            let exception = format!("ValueError: non-hexadecimal number found in fromhex() arg at position {position}");
            AmcError::Raised(MotionError::Native(exception).in_frame(VERIFICATION_FROMHEX))
        })?;
        // The decryption splits the data into blocks first.
        let plain = xtea::decrypt_ecb(&ANSWER_KEY, &cipher).ok_or_else(|| {
            let raised = MotionError::Native(
                "ValueError: Input 'bytestring' must be a multiple of block_size / segment_size (CFB mode) in length"
                    .into(),
            );
            AmcError::Raised(raised.in_frames([
                at(VERIFICATION, 202),
                at("AMC._decrypt", 141),
                PEP272_DECRYPT,
                PEP272_SPLIT_BLOCKS,
            ]))
        })?;
        Ok(plain == challenge.as_bytes())
    }

    /// Disconnects: dropping the link closes the port.
    pub fn close(self) {}
}

/// Whether `reply` is `prefix` followed by exactly the bytes of `echoed`.
fn echoes(reply: &[u8], prefix: &[u8], echoed: &str) -> bool {
    reply.starts_with(prefix) && &reply[prefix.len()..] == echoed.as_bytes()
}

/// The `':'`-separated fields of the third reply line, or `None` for an
/// undecodable reply or fewer than three lines.
fn auth_answer_fields(reply: Vec<u8>) -> Option<Vec<String>> {
    let text = String::from_utf8(reply).ok()?;
    let line = text.split("\r\n").nth(2)?;
    Some(line.split(':').map(str::to_string).collect())
}

/// Upper-case hexadecimal of `bytes`.
pub(super) fn upper_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::motion::testing::FakeClock;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A scripted controller: every write is recorded and may queue a reply.
    #[derive(Clone, Default)]
    struct FakeLink {
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        replies: Arc<Mutex<VecDeque<Vec<u8>>>>,
        name: String,
    }

    impl FakeLink {
        fn new(name: &str) -> Self {
            FakeLink { name: name.into(), ..Default::default() }
        }
        /// Queue a reply delivered after the next write.
        fn reply(&self, bytes: &[u8]) {
            self.replies.lock().unwrap().push_back(bytes.to_vec());
        }
        fn writes(&self) -> Vec<Vec<u8>> {
            self.written.lock().unwrap().clone()
        }
    }

    impl Link for FakeLink {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
            self.written.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
        fn read_available(&mut self) -> Result<Vec<u8>, LinkError> {
            Ok(self.replies.lock().unwrap().pop_front().unwrap_or_default())
        }
        fn clear_input(&mut self) -> Result<(), LinkError> {
            Ok(())
        }
        fn name(&self) -> &str {
            &self.name
        }
    }

    /// The reply bytes a genuine controller sends for `challenge` (both halves echoed, then `TX:<hex>`).
    fn genuine_auth_replies(link: &FakeLink, challenge: &str) {
        let enc = upper_hex(&xtea::encrypt_ecb(&REQUEST_KEY, challenge.as_bytes()).unwrap());
        link.reply(format!("1st half:{}", &enc[..16]).as_bytes());
        link.reply(format!("\r\n2st half:{}", &enc[16..]).as_bytes());
        let answer = upper_hex(&xtea::encrypt_ecb(&ANSWER_KEY, challenge.as_bytes()).unwrap());
        link.reply(format!("\r\nOK\r\nTX:{answer}\r\n").as_bytes());
    }

    const CHALLENGE: &str = "abcdefghABCDEFGH";

    /// Parking in standby writes exactly `Park` (utf-8, no LF CR).
    #[test]
    fn park_if_standby_writes_raw_park() {
        let link = FakeLink::new("COM1");
        let mut amc = Amc::new(Box::new(link.clone()));
        amc.park_if_standby().unwrap();
        assert_eq!(link.writes(), vec![b"Park".to_vec()]);
    }

    /// The handshake: exact frames (ZP + 16 hex + '0'/'1' + LF CR, then CMD88) and a
    /// genuine answer accepted. Hex values cross-checked with an independent XTEA implementation.
    #[test]
    fn verification_with_encryption_accepts_genuine_controller() {
        let link = FakeLink::new("COM1");
        genuine_auth_replies(&link, CHALLENGE);
        let mut amc = Amc::new(Box::new(link.clone()));
        let mut clock = FakeClock::default();
        assert!(amc.authenticate(CHALLENGE, &mut clock).unwrap());
        let w = link.writes();
        assert_eq!(w[0], b"ZPA6EC39D1DFA5CDCF0\n\r");
        assert_eq!(w[1], b"ZP7A210F8F7BE62FCB1\n\r");
        assert_eq!(w[2], b"CMD88");
        assert_eq!(clock.now(), Duration::from_millis(100 * 4 + 1100));
    }

    /// The answer encrypted with the second key must decrypt to the challenge.
    #[test]
    fn verification_with_encryption_answer_key() {
        let a = upper_hex(&xtea::encrypt_ecb(&ANSWER_KEY, CHALLENGE.as_bytes()).unwrap());
        assert_eq!(a, "55D93C26166062BD9F2746B442CCD21A");
    }

    /// A wrong first echo returns false before the second half is sent.
    #[test]
    fn verification_rejects_wrong_first_echo() {
        let link = FakeLink::new("COM1");
        link.reply(b"1st half:0000000000000000");
        let mut amc = Amc::new(Box::new(link.clone()));
        assert!(!amc.authenticate(CHALLENGE, &mut FakeClock::default()).unwrap());
        assert_eq!(link.writes().len(), 1);
    }

    /// A third line that is not `TX:` returns false; a third line missing returns false.
    #[test]
    fn verification_rejects_missing_or_foreign_answer() {
        for answer in ["\r\nOK\r\nRX:00\r\n", "\r\nonly two\r\n"] {
            let link = FakeLink::new("COM1");
            let enc = upper_hex(&xtea::encrypt_ecb(&REQUEST_KEY, CHALLENGE.as_bytes()).unwrap());
            link.reply(format!("1st half:{}", &enc[..16]).as_bytes());
            link.reply(format!("\r\n2st half:{}", &enc[16..]).as_bytes());
            link.reply(answer.as_bytes());
            let mut amc = Amc::new(Box::new(link));
            assert!(!amc.authenticate(CHALLENGE, &mut FakeClock::default()).unwrap(), "{answer:?}");
        }
    }

    /// `TX:` with a payload that is not hex is an error.
    #[test]
    fn verification_raises_on_bad_hex() {
        let link = FakeLink::new("COM1");
        let enc = upper_hex(&xtea::encrypt_ecb(&REQUEST_KEY, CHALLENGE.as_bytes()).unwrap());
        link.reply(format!("1st half:{}", &enc[..16]).as_bytes());
        link.reply(format!("\r\n2st half:{}", &enc[16..]).as_bytes());
        link.reply(b"\r\nOK\r\nTX:ZZ\r\n");
        let mut amc = Amc::new(Box::new(link));
        assert!(amc.authenticate(CHALLENGE, &mut FakeClock::default()).is_err());
    }

    /// A parameter save pauses 0.3 s on an MDBOX and 0.1 s otherwise; the firmware query keeps the first line.
    #[test]
    fn save_pause_and_firmware_line() {
        let link = FakeLink::new("COM1");
        let mut amc = Amc::new(Box::new(link.clone()));
        let mut clock = FakeClock::default();
        amc.write_parameter(Parameter::ParkPosition, &FieldValue::Int(7), true, &mut clock).unwrap();
        assert_eq!(clock.now(), Duration::from_millis(300));
        amc.write_parameter(Parameter::KlmMode, &FieldValue::Int(1), false, &mut clock).unwrap();
        assert_eq!(clock.now(), Duration::from_millis(400));
        link.reply(b"Thanos4U(M):v1\r\nmore");
        assert_eq!(amc.firmware(&mut clock).unwrap(), "Thanos4U(M):v1");
        assert_eq!(link.writes(), vec![b"spv04007".to_vec(), b"spv141".to_vec(), b"RQM".to_vec()]);
    }

    /// Hexadecimal text accepts whitespace between pairs only.
    #[test]
    fn fromhex_rules() {
        assert_eq!(python::bytes_fromhex("0aFF 10"), Ok(vec![0x0A, 0xFF, 0x10]));
        assert_eq!(python::bytes_fromhex("0a\u{b}\t ff"), Ok(vec![0x0A, 0xFF]), "the six ASCII whitespace characters");
        assert_eq!(python::bytes_fromhex("0 a"), Err(1));
        assert_eq!(python::bytes_fromhex("abc"), Err(3));
    }
}

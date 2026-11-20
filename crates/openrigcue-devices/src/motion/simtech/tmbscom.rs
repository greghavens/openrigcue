//! SimTech's typed interface to the shared TMBS serial protocol.
//!
//! Framing, stream reads, register parsing and actions belong to `motion::tmbs`;
//! this adapter adds SimTech's method names, error types and two-flag view.

use crate::motion::clock::Clock;
use crate::motion::python_print;
use crate::motion::tmbs::{self, Command, Register, TmbsLink};
use crate::serial::Link;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFlags {
    pub main_power_on: bool,
    pub homing_complete: bool,
}

impl StatFlags {
    pub fn from_bits(bits: i64) -> Self {
        tmbs::StatFlags::from_bits(bits).into()
    }
}

impl From<tmbs::StatFlags> for StatFlags {
    fn from(flags: tmbs::StatFlags) -> Self {
        Self { main_power_on: flags.main_power_on, homing_complete: flags.homing_complete }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TmbsError {
    #[error(transparent)]
    Parse(#[from] tmbs::ParseError),
    #[error("{0} - response is not hexadecimal")]
    BadHex(&'static str),
    #[error("position {0} does not fit a signed 32-bit integer")]
    PositionRange(i64),
    /// An error of the call (the formatting of a dynamic value).
    #[error("{0}")]
    Raised(String),
}

/// The TMBS protocol on one serial port.
pub struct TmbsInterface {
    link: TmbsLink,
}

impl TmbsInterface {
    pub fn new(link: Box<dyn Link>) -> Self {
        Self { link: TmbsLink::new(link) }
    }

    pub fn port(&self) -> &str {
        self.link.port()
    }

    fn read_register(&mut self, register: Register, clock: &mut dyn Clock) -> Result<i64, TmbsError> {
        self.link.read_register(0, register, clock).map_err(Into::into)
    }

    pub fn read_model_code(&mut self, clock: &mut dyn Clock) -> Result<i64, TmbsError> {
        self.read_register(Register::ModelCode, clock)
    }

    pub fn read_serial_number(&mut self, clock: &mut dyn Clock) -> Result<i64, TmbsError> {
        self.read_register(Register::SerialNumber, clock)
    }

    pub fn read_homing_pattern(&mut self, clock: &mut dyn Clock) -> Result<i64, TmbsError> {
        self.read_register(Register::HomingPattern, clock)
    }

    pub fn read_alarm_flags(&mut self, clock: &mut dyn Clock) -> Result<i64, TmbsError> {
        self.read_register(Register::Alarm, clock)
    }

    pub fn read_stat_flags(&mut self, clock: &mut dyn Clock) -> Result<Option<StatFlags>, TmbsError> {
        self.link.read_status(0, clock).map(|flags| flags.map(Into::into)).map_err(Into::into)
    }

    fn action(&mut self, command: Command, axis: u32, clock: &mut dyn Clock) -> Result<bool, TmbsError> {
        self.link.action_axis(axis, &command, clock).map_err(Into::into)
    }

    pub fn set_servo_on(&mut self, clock: &mut dyn Clock) -> Result<bool, TmbsError> {
        self.action(Command::Servo(true), 0, clock)
    }

    pub fn set_servo_off(&mut self, clock: &mut dyn Clock) -> Result<bool, TmbsError> {
        self.action(Command::Servo(false), 0, clock)
    }

    pub fn check_alarm(&mut self, clock: &mut dyn Clock) -> Result<bool, TmbsError> {
        self.action(Command::InternalStatusReset, 0, clock)
    }

    pub fn reset_alarm(&mut self, clock: &mut dyn Clock) -> Result<bool, TmbsError> {
        self.action(Command::StatusCheck, 0, clock)
    }

    pub fn cancel_movement(&mut self, clock: &mut dyn Clock) -> Result<bool, TmbsError> {
        self.action(Command::CancelMovement, 0, clock)
    }

    pub fn move_org(&mut self, axis: u32, mode: &str, clock: &mut dyn Clock) -> Result<bool, TmbsError> {
        self.action(Command::Homing(mode.into()), axis, clock)
    }

    pub fn write_velocity(
        &mut self,
        axis: u32,
        velocity: i64,
        acceleration: i64,
        clock: &mut dyn Clock,
    ) -> Result<bool, TmbsError> {
        self.action(Command::Velocity { speed: velocity, accel: acceleration }, axis, clock)
    }

    /// Writes the velocity and acceleration of `axis`, each as four or more lower-case hexadecimal digits, from integers of any width.
    pub fn write_velocity_native(
        &mut self,
        axis: u32,
        velocity: &openrigcue_model::native_value::LegacyValue,
        acceleration: &openrigcue_model::native_value::LegacyValue,
        clock: &mut dyn Clock,
    ) -> Result<bool, TmbsError> {
        let hex = |value| crate::motion::native::percent_hex(value, 4).map_err(|e| TmbsError::Raised(e.to_string()));
        let command = Command::VelocityHex { speed: hex(velocity)?, accel: hex(acceleration)? };
        self.action(command, axis, clock)
    }

    pub fn move_absolute(&mut self, axis: u32, position: i64) -> Result<bool, TmbsError> {
        let position = i32::try_from(position).map_err(|_| TmbsError::PositionRange(position))?;
        let sent = self.link.send_text(axis, &Command::MoveAbsolute(position).text());
        if !sent {
            python_print(&["move_abs - failed"]);
        }
        Ok(sent)
    }
}

// Helpers for the driver fixtures, using the same
// command builders rather than a second protocol implementation.
#[cfg(test)]
pub use fixture_names::*;

#[cfg(test)]
mod fixture_names {
    use super::*;
    pub use tmbs::{read_register_command, ETX, STX};
    pub const MODEL_CODE_REGISTER: &str = Register::ModelCode.address();
    pub const SERIAL_NUMBER_REGISTER: &str = Register::SerialNumber.address();
    pub const HOMING_PATTERN_REGISTER: &str = Register::HomingPattern.address();
    pub const STAT_REGISTER: &str = Register::Status.address();
    pub const ALRM_REGISTER: &str = Register::Alarm.address();
    pub const SERVO_ON: &str = "q1000000000";

    pub fn bcc(text: &[u8]) -> String {
        std::str::from_utf8(&tmbs::bcc(text)).unwrap().to_owned()
    }
    pub fn pack(message: &str, axis: u32) -> Vec<u8> {
        tmbs::pack_axis(axis, message)
    }
    pub fn unpack(message: &[u8]) -> Option<Vec<u8>> {
        tmbs::unpack(message).ok()
    }
    pub fn homing_command(mode: &str) -> String {
        Command::Homing(mode.into()).text()
    }
    pub fn velocity_command(velocity: i64, acceleration: i64) -> String {
        Command::Velocity { speed: velocity, accel: acceleration }.text()
    }
    pub fn absolute_move_command(position: i64) -> Option<String> {
        i32::try_from(position).ok().map(|position| Command::MoveAbsolute(position).text())
    }
    pub fn parse_python_hex(bytes: &[u8]) -> Option<i64> {
        tmbs::py_int_hex(bytes)
    }
    pub fn parse_direct_response(bytes: &[u8]) -> Option<[i64; 4]> {
        tmbs::parse_direct(bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_tmbs_adapter_keeps_register_status_and_action_error_details() {
        use crate::motion::driver_support::testing::ScriptedLink;
        use crate::motion::testing::FakeClock;

        let link = ScriptedLink::new("tmbs", |_| tmbs::testing::reply('0', "R4zz000000"));
        let mut interface = TmbsInterface::new(Box::new(link));
        let mut clock = FakeClock::default();
        for error in [
            interface.read_model_code(&mut clock).unwrap_err(),
            interface.read_serial_number(&mut clock).unwrap_err(),
            interface.read_homing_pattern(&mut clock).unwrap_err(),
            interface.read_alarm_flags(&mut clock).unwrap_err(),
            interface.read_stat_flags(&mut clock).unwrap_err(),
        ] {
            assert_eq!(error.to_string(), "invalid literal for int() with base 16: b'zz000000'");
            let TmbsError::Parse(error) = error else { panic!("native parse error must survive the adapter") };
            assert_eq!(error.input(), b"zz000000");
        }
        for error in [
            interface.set_servo_on(&mut clock).unwrap_err(),
            interface.set_servo_off(&mut clock).unwrap_err(),
            interface.check_alarm(&mut clock).unwrap_err(),
            interface.reset_alarm(&mut clock).unwrap_err(),
            interface.cancel_movement(&mut clock).unwrap_err(),
            interface.move_org(0, "07", &mut clock).unwrap_err(),
            interface.write_velocity(0, 10000, 80, &mut clock).unwrap_err(),
        ] {
            assert_eq!(error.to_string(), "invalid literal for int() with base 16: b'zz'");
            let TmbsError::Parse(error) = error else { panic!("native action parse error must survive the adapter") };
            assert_eq!(error.input(), b"zz");
        }
    }

    /// Framing and check code of an absolute move of axis 0 to -5000: byte sum 0x300 → BCC "00".
    #[test]
    fn pack_absolute_move() {
        let cmd = absolute_move_command(-5000).unwrap();
        assert_eq!(cmd, "aFFFFEC7800");
        assert_eq!(pack(&cmd, 0), b"\x020aFFFFEC780000\x03");
        assert!(absolute_move_command(1 << 31).is_none());
    }

    /// The check code: ((sum & 0xFF) ^ 0xFF) + 1, masked, upper-case.
    #[test]
    fn bcc_twos_complement() {
        assert_eq!(bcc(b"0R4000068000"), "8C");
        assert_eq!(bcc(b"U0R4808976450"), "16");
        assert_eq!(bcc(b"\x01"), "FF");
        assert_eq!(bcc(b""), "00");
    }

    /// Unpacking accepts the reply shape of the scripted link (U, axis, payload, BCC over 'U'+axis+payload).
    #[test]
    fn unpack_mock_reply() {
        let body = b"U0R4808976450";
        let mut msg = vec![STX];
        msg.extend_from_slice(body);
        msg.extend_from_slice(bcc(body).as_bytes());
        msg.push(ETX);
        assert_eq!(unpack(&msg), Some(b"R4808976450".to_vec()));
        let mut bad = msg.clone();
        bad[5] ^= 1;
        assert_eq!(unpack(&bad), None, "bcc wrong");
        assert_eq!(unpack(&msg[..12]), None, "too short");
    }

    /// Command builders: register read, homing, velocity (four lower-case hexadecimal digits).
    #[test]
    fn command_strings() {
        assert_eq!(read_register_command(MODEL_CODE_REGISTER), "R4000068000");
        assert_eq!(homing_command("07"), "o0700000000");
        assert_eq!(velocity_command(10000, 80), "v2271000500");
        assert_eq!(velocity_command(0x1abcd, 1), "v21abcd00010");
        assert_eq!(velocity_command(-5, 1), "v2-00500010");
    }

    /// The payload parsed as a hexadecimal integer.
    #[test]
    fn python_hex_parsing() {
        assert_eq!(parse_python_hex(b"808976450"), Some(0x8_0897_6450));
        assert_eq!(parse_python_hex(b"0000000B"), Some(11));
        assert_eq!(parse_python_hex(b"0x1f"), Some(31));
        assert_eq!(parse_python_hex(b" 1_f "), Some(31));
        assert_eq!(parse_python_hex(b"zz"), None);
        assert_eq!(parse_python_hex(b""), None);
        assert_eq!(parse_python_hex(b"-8000000000000000"), Some(i64::MIN));
        assert_eq!(parse_python_hex(b"\x0b0X_f\x0c"), Some(15));
        assert_eq!(parse_python_hex(b"\xc2\xa01"), None);
        assert_eq!(parse_python_hex(b"1\xc2\xa0"), None);
    }

    /// Status reply parsing: short replies give zeros, hex pairs otherwise.
    #[test]
    fn direct_response() {
        assert_eq!(parse_direct_response(b"R400"), Some([0; 4]));
        assert_eq!(parse_direct_response(b"R40B000102"), Some([0x0B, 0, 1, 2]));
        assert_eq!(parse_direct_response(b"R4zz000102"), None);
    }
}

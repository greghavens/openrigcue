//! Pure parts of the Yaw VR protocol: discovery replies, TCP commands,
//! the parameter dump, the UDP angle message and the feedback datagrams, plus the number
//! formatting the text messages depend on.

use std::fmt;

/// Broadcast query of the discovery.
pub const CALLING: &[u8] = b"YAW_CALLING";
/// Local port the discovery binds, and the port devices listen on for calls and angle messages.
pub const DISCOVERY_BIND_PORT: u16 = 22_222;
pub const DEVICE_UDP_PORT: u16 = 50_010;
/// Socket timeouts of discovery, feedback and the TCP control channel.
pub const SOCKET_TIMEOUT_SECS: u64 = 7;
/// Receive buffer sizes: 8192 bytes in general; the parameter dump is read with 200.
pub const RECV_SIZE: usize = 8192;
pub const PARAMETERS_RECV_SIZE: usize = 200;

/// Byte 161: start streaming.
pub const START: [u8; 1] = [0xA1];
/// Byte 162: stop.
pub const STOP: [u8; 1] = [0xA2];
/// Bytes 162, 1: stop and park.
pub const STOP_AND_PARK: [u8; 2] = [0xA2, 0x01];
/// Byte 163: exit, sent on disconnect.
pub const EXIT: [u8; 1] = [0xA3];
/// Bytes 85, 1: centre (homing).
pub const CENTER: [u8; 2] = [0x55, 0x01];
/// Byte 246: request the parameter dump.
pub const GET_PARAMETERS: [u8; 1] = [0xF6];

/// The hello of a connect: byte 48, the UDP port as a big-endian signed 32-bit integer, then the three
/// bytes `SRS`. `None` for a port outside a signed 32-bit integer.
pub fn hello(udp_port: i64) -> Option<Vec<u8>> {
    let port = i32::try_from(udp_port).ok()?;
    let mut out = vec![48];
    out.extend_from_slice(&port.to_be_bytes());
    out.extend_from_slice(b"SRS");
    Some(out)
}

/// One discovered platform; every field is the text of the reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YawDevice {
    pub model: String,
    pub uuid: String,
    pub name: String,
    pub tcp_port: String,
    pub status: String,
}

impl YawDevice {
    /// The device as the log prints it: `{'DEVICE_MODEL': …, 'UUID': …, 'DEVICE_NAME': …,
    /// 'TCP_PORT': …, 'STATUS': …}`.
    pub fn python_str(&self) -> String {
        crate::motion::python::str_dict_repr(&[
            ("DEVICE_MODEL", &self.model),
            ("UUID", &self.uuid),
            ("DEVICE_NAME", &self.name),
            ("TCP_PORT", &self.tcp_port),
            ("STATUS", &self.status),
        ])
    }
}

/// A discovery reply: `';'`-separated fields taken by position as
/// `DEVICE_MODEL, UUID, DEVICE_NAME, TCP_PORT, STATUS`; a sixth field (the IP) and any further
/// fields are ignored. `None` for
/// fewer than five fields or non-ASCII text.
pub fn parse_discovery_reply(data: &[u8]) -> Option<YawDevice> {
    if !data.is_ascii() {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let f: Vec<&str> = text.split(';').collect();
    let field = |i: usize| f.get(i).map(|s| s.to_string());
    Some(YawDevice { model: field(0)?, uuid: field(1)?, name: field(2)?, tcp_port: field(3)?, status: field(4)? })
}

/// The platform limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YawLimits {
    pub power: i64,
    pub pitch_forward: i64,
    pub pitch_backward: i64,
    pub roll: i64,
    pub yaw: i64,
    pub yaw_limit_enabled: i64,
    pub vibration_enabled: i64,
}

impl YawLimits {
    /// Values a connect sets for the `YAW_EMULATOR` device.
    pub const EMULATOR: YawLimits = YawLimits {
        power: 70,
        pitch_forward: 30,
        pitch_backward: 30,
        roll: 30,
        yaw: 359,
        yaw_limit_enabled: 1,
        vibration_enabled: 1,
    };

    /// The sum of the seven values, by which a change of the limits is noticed.
    pub fn sum(&self) -> i64 {
        self.power
            + self.pitch_forward
            + self.pitch_backward
            + self.roll
            + self.yaw
            + self.yaw_limit_enabled
            + self.vibration_enabled
    }

    /// The larger of the forward and backward pitch limits (the backward one when they are equal).
    pub fn pitch_limit(&self) -> i64 {
        if self.pitch_forward > self.pitch_backward {
            self.pitch_forward
        } else {
            self.pitch_backward
        }
    }

    /// The limits in the first 32 bytes of a parameter dump: signed bytes at fixed offsets and one big-endian
    /// unsigned 32-bit integer.
    pub fn unpack(data: &[u8]) -> Option<YawLimits> {
        let d = data.get(..32)?;
        let b = |i: usize| i64::from(d[i] as i8);
        Some(YawLimits {
            power: b(4),
            pitch_backward: b(9),
            pitch_forward: b(13),
            roll: b(17),
            yaw: i64::from(u32::from_be_bytes([d[19], d[20], d[21], d[22]])),
            yaw_limit_enabled: b(24),
            vibration_enabled: b(31),
        })
    }

    /// The five commands that set the limits, in the order they are sent, each with the name its response is
    /// logged under. A command whose value does not fit its field holds the range error's
    /// text; the commands before it are still sent.
    pub fn limit_commands(&self) -> [(&'static str, Result<Vec<u8>, &'static str>); 5] {
        const I_RANGE: &str = "'i' format requires -2147483648 <= number <= 2147483647";
        const B_RANGE: &str = "byte format requires -128 <= number <= 127";
        let i32_be = |v: i64| i32::try_from(v).map(i32::to_be_bytes).map_err(|_| I_RANGE);
        let i8_byte = |v: i64| i8::try_from(v).map(|b| b as u8).map_err(|_| B_RANGE);
        let with = |code: u8, parts: &[&[u8]]| -> Vec<u8> {
            let mut out = vec![code];
            for p in parts {
                out.extend_from_slice(p);
            }
            out
        };
        [
            ("power response is ", i32_be(self.power).map(|p| with(50, &[&p]))),
            (
                "pitch fwd, bwd and row response is ",
                (|| Ok(with(64, &[&i32_be(self.pitch_backward)?, &i32_be(self.pitch_forward)?, &i32_be(self.roll)?])))(
                ),
            ),
            ("yaw limit enabled response is ", i8_byte(self.yaw_limit_enabled).map(|b| with(98, &[&[b]]))),
            ("yaw response is ", i32_be(self.yaw).map(|y| with(112, &[&y]))),
            ("vibration enabled response is ", i8_byte(self.vibration_enabled).map(|b| with(97, &[&[b]]))),
        ]
    }

    /// The five commands that set the limits, in order. `None` when a value does not fit its field.
    pub fn commands(&self) -> Option<[Vec<u8>; 5]> {
        let [a, b, c, d, e] = self.limit_commands().map(|(_, command)| command.ok());
        Some([a?, b?, c?, d?, e?])
    }
}

/// A number as the messages print it: integers without a decimal point, floats as [`float_repr`] gives them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PyNumber {
    Int(i64),
    Float(f64),
}

impl PyNumber {
    pub fn value(self) -> f64 {
        match self {
            PyNumber::Int(i) => i as f64,
            PyNumber::Float(f) => f,
        }
    }
}

impl fmt::Display for PyNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PyNumber::Int(i) => write!(f, "{i}"),
            PyNumber::Float(x) => f.write_str(&float_repr(*x)),
        }
    }
}

/// The text of a float: the shortest round-trip digits, fixed notation when the decimal exponent is in
/// `-4 < decpt <= 16`, otherwise `d.ddde±XX`; integral values keep `.0`.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{:e}", x.abs());
    let (mantissa, exponent) = sci.split_once('e').expect("LowerExp has an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let decpt = exponent.parse::<i32>().expect("integer exponent") + 1;
    let sign = if x < 0.0 { "-" } else { "" };
    let body = if decpt > -4 && decpt <= 16 {
        let n = digits.len() as i32;
        if decpt <= 0 {
            format!("0.{}{digits}", "0".repeat((-decpt) as usize))
        } else if decpt >= n {
            format!("{digits}{}.0", "0".repeat((decpt - n) as usize))
        } else {
            format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
        }
    } else {
        let tail = if digits.len() > 1 { format!(".{}", &digits[1..]) } else { String::new() };
        let e = decpt - 1;
        format!("{}{tail}e{}{:02}", &digits[..1], if e < 0 { '-' } else { '+' }, e.abs())
    };
    format!("{sign}{body}")
}

/// `x` rounded to `decimals` decimal places.
pub fn round_to(x: f64, decimals: usize) -> f64 {
    format!("{x:.decimals$}").parse().unwrap_or(x)
}

/// The UDP angle message of a motion frame:
/// `Y[yaw]P[pitch]R[roll]V[amp,amp,amp,hz]F[pwm,pwm]`.
pub fn angle_message(yaw: PyNumber, pitch: f64, roll: f64, amplitude: i64, hz: i64, pwm: i64) -> String {
    format!(
        "Y[{yaw}]P[{}]R[{}]V[{amplitude},{amplitude},{amplitude},{hz}]F[{pwm},{pwm}]",
        float_repr(pitch),
        float_repr(roll)
    )
}

/// Orientation report of the platform (`YAW`, `PITCH`, `ROLL`, `BATTERY`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct YawReport {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
    pub battery: f64,
}

/// `text` as a float: surrounding whitespace allowed, `inf`/`nan` spellings accepted.
fn python_float(text: &str) -> Option<f64> {
    openrigcue_model::native_value::parse_py_float(text)
}

/// A feedback datagram: a message starting `SY` has `SY[`, `SP[`, `SR[`, `U[`
/// removed, is split at `]`, and the first four fields become YAW, PITCH, ROLL, BATTERY (0 for a field
/// that is not a number or that is missing). `None` for any other message ("Unexpected message").
pub fn parse_report(message: &str) -> Option<YawReport> {
    if !message.starts_with("SY") {
        return None;
    }
    let cleaned = message.replace("SY[", "").replace("SP[", "").replace("SR[", "").replace("U[", "");
    let parts: Vec<&str> = cleaned.split(']').collect();
    let value = |i: usize| parts.get(i).and_then(|p| python_float(p)).unwrap_or(0.0);
    Some(YawReport { yaw: value(0), pitch: value(1), roll: value(2), battery: value(3) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hello: byte 48, the big-endian UDP port, the bytes `SRS`.
    #[test]
    fn hello_layout() {
        assert_eq!(hello(33333).unwrap(), [0x30, 0x00, 0x00, 0x82, 0x35, b'S', b'R', b'S']);
        assert!(hello(1 << 40).is_none());
    }

    /// Discovery replies: fields by position, so five are needed and a sixth is
    /// ignored.
    #[test]
    fn discovery_reply_mapping() {
        let d = parse_discovery_reply(b"YAWDEVICE;1234;MyYaw;50020;READY;192.168.1.9").unwrap();
        assert_eq!((d.model.as_str(), d.name.as_str(), d.tcp_port.as_str()), ("YAWDEVICE", "MyYaw", "50020"));
        let five = parse_discovery_reply(b"YAWDEVICE;1234;MyYaw;50020;READY").unwrap();
        assert_eq!(
            five,
            YawDevice {
                model: "YAWDEVICE".into(),
                uuid: "1234".into(),
                name: "MyYaw".into(),
                tcp_port: "50020".into(),
                status: "READY".into(),
            }
        );
        assert!(parse_discovery_reply(b"YAWDEVICE;1234;MyYaw;50020").is_none());
        assert!(parse_discovery_reply(b"YAWDEVICE;1234;MyYaw").is_none());
        assert!(parse_discovery_reply("Y\u{e9};a;b;c;d".as_bytes()).is_none(), "decode('ascii') fails");
    }

    /// The parameter dump: fixed offsets in the first 32 bytes, signed bytes.
    #[test]
    fn parameter_dump_unpack() {
        let mut d = [0u8; 32];
        d[4] = 80;
        d[9] = 25;
        d[13] = 0xF6;
        d[17] = 30;
        d[19..23].copy_from_slice(&359u32.to_be_bytes());
        d[24] = 1;
        d[31] = 0;
        let l = YawLimits::unpack(&d).unwrap();
        assert_eq!(
            l,
            YawLimits {
                power: 80,
                pitch_forward: -10,
                pitch_backward: 25,
                roll: 30,
                yaw: 359,
                yaw_limit_enabled: 1,
                vibration_enabled: 0
            }
        );
        assert_eq!(l.sum(), 80 + 25 - 10 + 30 + 359 + 1);
        assert!(YawLimits::unpack(&d[..31]).is_none());
    }

    /// The limit commands: 50 with one int32, 64 with three, 98 with a byte, 112 with one int32, 97 with a byte.
    #[test]
    fn set_limits_commands() {
        let c = YawLimits::EMULATOR.commands().unwrap();
        assert_eq!(c[0], [50, 0, 0, 0, 70]);
        assert_eq!(c[1], [64, 0, 0, 0, 30, 0, 0, 0, 30, 0, 0, 0, 30]);
        assert_eq!(c[2], [98, 1]);
        assert_eq!(c[3], [112, 0, 0, 1, 0x67]);
        assert_eq!(c[4], [97, 1]);
        assert!(YawLimits { yaw_limit_enabled: 200, ..YawLimits::EMULATOR }.commands().is_none());
    }

    /// The text of floats and integers as used in the angle message.
    #[test]
    fn python_number_text() {
        for (x, s) in [
            (12.5, "12.5"),
            (1.0, "1.0"),
            (-0.0, "-0.0"),
            (0.0001, "0.0001"),
            (1e-5, "1e-05"),
            (1e16, "1e+16"),
            (1234567890123456.0, "1234567890123456.0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (-1.5e-7, "-1.5e-07"),
            (123456789012345680000.0, "1.2345678901234568e+20"),
        ] {
            assert_eq!(float_repr(x), s);
        }
        assert_eq!(PyNumber::Int(0).to_string(), "0");
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(-0.00001, 4).to_string(), "-0");
    }

    /// The angle message layout.
    #[test]
    fn angle_message_layout() {
        let m = angle_message(PyNumber::Float(359.5), -12.25, 0.0, 40, 17, 255);
        assert_eq!(m, "Y[359.5]P[-12.25]R[0.0]V[40,40,40,17]F[255,255]");
    }

    /// Feedback: SY[..]SP[..]SR[..]U[..] reports; anything else is unexpected.
    #[test]
    fn feedback_report() {
        let r = parse_report("SY[12.5]SP[-3]SR[0.25]U[87]").unwrap();
        assert_eq!(r, YawReport { yaw: 12.5, pitch: -3.0, roll: 0.25, battery: 87.0 });
        let r = parse_report("SY[x]SP[1]").unwrap();
        assert_eq!((r.yaw, r.pitch, r.battery), (0.0, 1.0, 0.0));
        assert!(parse_report("HELLO").is_none());
        let r = parse_report("SY[1_2.5]SP[+1.2_5]SR[1_.0]U[Infinity]").unwrap();
        assert_eq!((r.yaw, r.pitch, r.roll), (12.5, 1.25, 0.0));
        assert!(r.battery.is_infinite());
        let r = parse_report("SY[-0]SP[1e999]SR[\u{1c}3]U[nAn]").unwrap();
        assert_eq!(r.yaw.to_bits(), (-0.0_f64).to_bits());
        assert!(r.pitch.is_infinite());
        assert_eq!(r.roll, 0.0);
        assert!(r.battery.is_nan());
    }
}

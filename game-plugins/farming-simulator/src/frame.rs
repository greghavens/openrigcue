//! The text frame `TelemetryMod:buildTelemetry()` writes to the pipe.
//!
//! Frame = `"{"` + one `"<name>": <value>, ` piece per value + `"}"`, built in
//! a Lua table and joined with `table.concat`, then written with one
//! `file:write` and a `file:flush`.  Pieces come from
//! `string.format('"%s": %.4f, ', name, value)`; a `nil` value produces no
//! piece (`addFloatToTelemetry` checks `value ~= nil`).
//!
//! Order (identical in the FS19/FS22 and FS25 scripts):
//! * controlled vehicle, not broken: for each `vehicle.components[k]`
//!   (`ipairs`, k = 1, 2, ...) the 22 keys of [`COMPONENT_KEYS`] suffixed
//!   with `k`, then `rpm`, `rpmReal`, `speed`;
//! * controlled vehicle with `isBroken` true: `{}`;
//! * no vehicle: the 47 keys of [`NO_VEHICLE_KEYS`], all `0.0000`.

use std::fmt::Write as _;

/// Per-component keys (without the component index suffix), in write order:
/// `getAngularVelocity`, `getLinearVelocity`, `getWorldTranslation`,
/// `getWorldRotation`, `getTranslation`, `getRotation` (x, y, z each) and
/// `getQuaternion` (x, y, z, w) of `component.node`.
pub const COMPONENT_KEYS: [&str; 22] = [
    "avX",
    "avY",
    "avZ",
    "lvX",
    "lvY",
    "lvZ",
    "transWorldX",
    "transWorldY",
    "transWorldZ",
    "rotWorldX",
    "rotWorldY",
    "rotWorldZ",
    "transX",
    "transY",
    "transZ",
    "rotX",
    "rotY",
    "rotZ",
    "qx",
    "qy",
    "qz",
    "qw",
];

/// Keys written (value 0) when there is no controlled vehicle, in order.
pub const NO_VEHICLE_KEYS: [&str; 47] = [
    "avX1",
    "avY1",
    "avZ1",
    "avX2",
    "avY2",
    "avZ2",
    "lvX1",
    "lvY1",
    "lvZ1",
    "lvX2",
    "lvY2",
    "lvZ2",
    "transWorldX1",
    "transWorldY1",
    "transWorldZ1",
    "transWorldX2",
    "transWorldY2",
    "transWorldZ2",
    "rotWorldX1",
    "rotWorldY1",
    "rotWorldZ1",
    "rotWorldX2",
    "rotWorldY2",
    "rotWorldZ2",
    "transX1",
    "transY1",
    "transZ1",
    "transX2",
    "transY2",
    "transZ2",
    "rotX1",
    "rotY1",
    "rotZ1",
    "rotX2",
    "rotY2",
    "rotZ2",
    "rpm",
    "rpmReal",
    "speed",
    "qx1",
    "qy1",
    "qz1",
    "qw1",
    "qx2",
    "qy2",
    "qz2",
    "qw2",
];

/// Engine values for one vehicle component, in [`COMPONENT_KEYS`] order.
/// `None` stands for a Lua `nil` result (the key is then omitted).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComponentSample {
    pub values: [Option<f64>; 22],
}

impl ComponentSample {
    /// A component whose engine calls all returned numbers.
    pub fn from_values(v: [f64; 22]) -> Self {
        Self { values: v.map(Some) }
    }
}

/// What the mod reads from the controlled vehicle in one update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VehicleSample {
    /// `vehicle.isBroken` (Lua truthiness).
    pub is_broken: bool,
    /// `vehicle.components`, in `ipairs` order.
    pub components: Vec<ComponentSample>,
    /// `vehicle:getMotor():getLastMotorRpm()`.
    pub rpm: Option<f64>,
    /// `vehicle:getMotor():getLastRealMotorRpm()`.
    pub rpm_real: Option<f64>,
    /// `vehicle:getLastSpeed()`.
    pub speed: Option<f64>,
}

/// C `printf("%.4f")`, which is what Lua 5.1 `string.format` delegates to.
///
/// Finite values: exact decimal expansion, correctly rounded with ties to
/// even, sign kept for negative values that round to zero ("-0.0000"). Every
/// C runtime and Rust's formatter agree on finite doubles (checked against
/// 3047 Lua 5.1 vectors in `tests/frame_golden.rs`).
///
/// Non-finite values (only if the engine returns them) are spelled as the
/// game's C runtime does. The game is a Win32 program on the MSVC Universal
/// CRT, whose `%f` prints `inf`, `-inf`, `nan`, `-nan`, `-nan(ind)` for the
/// x87/SSE indefinite NaN (sign set, quiet, zero payload) and `nan(snan)` /
/// `-nan(snan)` for a signalling NaN.
pub fn format_fixed4(value: f64) -> String {
    if value.is_nan() {
        let bits = value.to_bits();
        let negative = bits >> 63 == 1;
        let quiet = bits & (1 << 51) != 0;
        let payload = bits & ((1 << 51) - 1);
        let sign = if negative { "-" } else { "" };
        let suffix = if !quiet {
            "(snan)"
        } else if negative && payload == 0 {
            "(ind)"
        } else {
            ""
        };
        return format!("{sign}nan{suffix}");
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf".into() } else { "inf".into() };
    }
    format!("{value:.4}")
}

/// One `string.format('"%s": %.4f, ', name, value)` piece.
pub fn push_pair(out: &mut String, name: &str, value: Option<f64>) {
    if let Some(v) = value {
        let _ = write!(out, "\"{name}\": {}, ", format_fixed4(v));
    }
}

/// The frame for one update. `vehicle` is `None` when there is no controlled
/// vehicle (`g_currentMission.controlledVehicle` / FS25 player's
/// `getCurrentVehicle()` returned nil).
pub fn encode_frame(vehicle: Option<&VehicleSample>) -> String {
    let mut out = String::with_capacity(2048);
    out.push('{');
    match vehicle {
        Some(v) => {
            if !v.is_broken {
                for (i, c) in v.components.iter().enumerate() {
                    let k = i + 1;
                    for (key, value) in COMPONENT_KEYS.iter().zip(c.values.iter()) {
                        push_pair(&mut out, &format!("{key}{k}"), *value);
                    }
                }
                push_pair(&mut out, "rpm", v.rpm);
                push_pair(&mut out, "rpmReal", v.rpm_real);
                push_pair(&mut out, "speed", v.speed);
            }
        }
        None => {
            for key in NO_VEHICLE_KEYS {
                push_pair(&mut out, key, Some(0.0));
            }
        }
    }
    out.push('}');
    out
}

/// Why a frame could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Byte offset and what was expected there.
    Expected(usize, &'static str),
    /// A value that is not a decimal number literal (e.g. `nan`, `inf`).
    BadNumber(usize),
    /// Bytes after the closing brace.
    Trailing(usize),
}

/// Decode a frame into `(name, value)` pairs in order of first appearance.
///
/// Accepts the grammar the mod produces and, more leniently, optional
/// whitespace around tokens, an optional trailing comma, double- or
/// single-quoted keys without escapes, and signed decimal numbers with an
/// optional exponent; a repeated key keeps its first position and takes the
/// last value. Names such as `nan`/`inf` are rejected.
pub fn decode_frame(text: &str) -> Result<Vec<(String, f64)>, DecodeError> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let ws = |i: &mut usize| {
        while *i < b.len() && (b[*i] as char).is_ascii_whitespace() {
            *i += 1;
        }
    };
    ws(&mut i);
    if b.get(i) != Some(&b'{') {
        return Err(DecodeError::Expected(i, "'{'"));
    }
    i += 1;
    let mut pairs: Vec<(String, f64)> = Vec::new();
    loop {
        ws(&mut i);
        match b.get(i) {
            Some(b'}') => {
                i += 1;
                break;
            }
            Some(&q) if q == b'"' || q == b'\'' => {
                let start = i + 1;
                let end = b[start..]
                    .iter()
                    .position(|&c| c == q || c == b'\\')
                    .map(|p| start + p)
                    .ok_or(DecodeError::Expected(start, "closing quote"))?;
                if b[end] == b'\\' {
                    return Err(DecodeError::Expected(end, "key without escapes"));
                }
                let key = text[start..end].to_owned();
                i = end + 1;
                ws(&mut i);
                if b.get(i) != Some(&b':') {
                    return Err(DecodeError::Expected(i, "':'"));
                }
                i += 1;
                ws(&mut i);
                let nstart = i;
                if matches!(b.get(i), Some(b'-') | Some(b'+')) {
                    i += 1;
                }
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    // exponent sign
                    if (b[i] == b'e' || b[i] == b'E') && matches!(b.get(i + 1), Some(b'-') | Some(b'+')) {
                        i += 1;
                    }
                    i += 1;
                }
                let lit = &text[nstart..i];
                let value = parse_decimal(lit).ok_or(DecodeError::BadNumber(nstart))?;
                if let Some(p) = pairs.iter_mut().find(|(k, _)| *k == key) {
                    p.1 = value;
                } else {
                    pairs.push((key, value));
                }
                ws(&mut i);
                match b.get(i) {
                    Some(b',') => i += 1,
                    Some(b'}') => {}
                    _ => return Err(DecodeError::Expected(i, "',' or '}'")),
                }
            }
            _ => return Err(DecodeError::Expected(i, "key or '}'")),
        }
    }
    ws(&mut i);
    if i != b.len() {
        return Err(DecodeError::Trailing(i));
    }
    Ok(pairs)
}

/// Decimal literal (digits, optional fraction, optional exponent, optional
/// leading sign); no names, hex or underscores.
fn parse_decimal(lit: &str) -> Option<f64> {
    let body = lit.strip_prefix(['-', '+']).unwrap_or(lit);
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(p) => (&body[..p], Some(&body[p + 1..])),
        None => (body, None),
    };
    let digits = mant.bytes().filter(|c| c.is_ascii_digit()).count();
    let dots = mant.bytes().filter(|&c| c == b'.').count();
    if digits == 0 || dots > 1 || digits + dots != mant.len() {
        return None;
    }
    if let Some(e) = exp {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        if e.is_empty() || !e.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    lit.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_vehicle_is_empty_braces() {
        let v = VehicleSample { is_broken: true, rpm: Some(1.0), ..Default::default() };
        assert_eq!(encode_frame(Some(&v)), "{}");
    }

    #[test]
    fn vehicle_without_components_writes_motor_values_only() {
        let v = VehicleSample { rpm: Some(850.0), rpm_real: Some(-0.00004), speed: Some(12.5), ..Default::default() };
        assert_eq!(encode_frame(Some(&v)), "{\"rpm\": 850.0000, \"rpmReal\": -0.0000, \"speed\": 12.5000, }");
    }

    #[test]
    fn nil_values_are_skipped() {
        let mut c = ComponentSample::from_values([1.0; 22]);
        c.values[21] = None;
        let v =
            VehicleSample { components: vec![c], rpm: None, rpm_real: Some(2.0), speed: None, ..Default::default() };
        let f = encode_frame(Some(&v));
        assert!(f.contains("\"qz1\": 1.0000, \"rpmReal\": 2.0000, }"));
        assert!(!f.contains("qw1"));
    }

    #[test]
    fn decode_roundtrip_and_python_semantics() {
        let f = encode_frame(None);
        let d = decode_frame(&f).unwrap();
        assert_eq!(d.len(), 47);
        assert!(d.iter().zip(NO_VEHICLE_KEYS).all(|((k, v), e)| k == e && *v == 0.0));
        assert_eq!(decode_frame("{}").unwrap(), vec![]);
        assert_eq!(
            decode_frame(" { 'a' : -1.5e+2 , \"b\": 2, \"a\": 3 } ").unwrap(),
            vec![("a".into(), 3.0), ("b".into(), 2.0)]
        );
        assert_eq!(decode_frame("{\"a\": nan, }"), Err(DecodeError::BadNumber(6)));
        assert_eq!(decode_frame("{\"a\": inf, }"), Err(DecodeError::BadNumber(6)));
        assert_eq!(decode_frame("{\"a\": 1, }x"), Err(DecodeError::Trailing(10)));
        assert!(decode_frame("{\"a\" 1}").is_err());
        assert!(decode_frame("{\"a\": 1 \"b\": 2}").is_err());
    }

    #[test]
    fn non_finite_spellings_follow_the_msvc_ucrt() {
        assert_eq!(format_fixed4(f64::INFINITY), "inf");
        assert_eq!(format_fixed4(f64::NEG_INFINITY), "-inf");
        // positive quiet NaN, zero payload
        assert_eq!(format_fixed4(f64::from_bits(0x7ff8_0000_0000_0000)), "nan");
        // the x86 "indefinite" NaN (0/0, inf-inf, sqrt(-1) on SSE)
        assert_eq!(format_fixed4(f64::from_bits(0xfff8_0000_0000_0000)), "-nan(ind)");
        // negative quiet NaN with a payload
        assert_eq!(format_fixed4(f64::from_bits(0xfff8_0000_0000_0001)), "-nan");
        assert_eq!(format_fixed4(f64::from_bits(0x7ff8_0000_0000_0001)), "nan");
        // signalling NaNs
        assert_eq!(format_fixed4(f64::from_bits(0x7ff0_0000_0000_0001)), "nan(snan)");
        assert_eq!(format_fixed4(f64::from_bits(0xfff0_0000_0000_0001)), "-nan(snan)");
        // none of these spellings decodes
        for s in ["inf", "-inf", "nan", "-nan(ind)", "nan(snan)"] {
            assert!(decode_frame(&format!("{{\"a\": {s}, }}")).is_err(), "{s}");
        }
    }
}

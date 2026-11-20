//! The Arduino CmdMessenger serial protocol, host side
//! (Intellibox, belt ESP32 bridge, HaptiCR).
//!
//! Frame: `<command index as ASCII decimal>` then `,<field>` per argument, terminated by `;`. Fields are
//! binary little-endian (`b` u8, `i` i16, `l` i32) or ASCII text (`s`); the bytes `,` `;` `/` and NUL inside
//! a field are prefixed with the escape byte `/`.

use std::io;

use super::link::SerialLink;

const FIELD_SEPARATOR: u8 = b',';
const COMMAND_SEPARATOR: u8 = b';';
const ESCAPE: u8 = b'/';

/// Wire format of one argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// `s`: ASCII text, no terminator.
    Text,
    /// `b`: unsigned byte.
    Byte,
    /// `i`: signed 16-bit little-endian.
    Int16,
    /// `l`: signed 32-bit little-endian.
    Int32,
}

/// One entry of a device's command table; its position in the table is the index sent on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub fields: &'static [Field],
}

/// A value to send.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// Truncated to an integer for the binary formats.
    Number(f64),
    /// ASCII in a text field; converted to an integer in a binary integer field.
    Text(String),
    /// Retained until the binary field's integer conversion.
    Native(openrigcue_model::native_value::LegacyValue),
}

/// A received argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Text(String),
    /// Raw bytes of an argument of an unknown command (no caller reads it).
    Raw(Vec<u8>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// The integer value of a received argument: an integer stays as it is, text is parsed
/// ([`super::numeric::python_int_checked`]).
pub fn python_int_arg(value: &Value) -> Result<i64, super::numeric::PyIntError> {
    match value {
        Value::Int(v) => Ok(*v),
        Value::Text(s) => super::numeric::python_int_checked(s),
        Value::Raw(_) => Err(super::numeric::PyIntError::Invalid),
    }
}

/// A received command: its table index (`None` for an index that is not a number,
/// an unknown command) and its decoded arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub command: Option<usize>,
    pub args: Vec<Value>,
}

impl Message {
    /// Whether this is `table[index]`.
    pub fn is(&self, index: usize) -> bool {
        self.command == Some(index)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CmdError {
    /// Sending: the format count differs from the argument count.
    #[error("Number of argument formats must match the number of arguments.")]
    ArgumentCount,
    /// Receiving: the field count differs from the command's formats.
    #[error("Number of argument formats must match the number of recieved arguments.")]
    ReceivedArgumentCount,
    /// The integer does not fit its binary field; `value` is its decimal text.
    #[error("Value {value} exceeds the size of the board's {field}.")]
    Overflow { value: String, field: &'static str },
    /// The value did not convert to an integer; the text is `Class: message`.
    #[error("{}", .0.split_once(": ").map_or(.0.as_str(), |(_, message)| message))]
    Native(String),
    #[error("text argument is not ASCII")]
    NotAscii,
    #[error("Incomplete message ({0:?})")]
    Incomplete(Vec<u8>),
    #[error("received command index {0} is not in the command table")]
    UnknownIndex(i64),
    #[error("field of {got} bytes cannot be decoded as {field:?}")]
    BadField { field: Field, got: usize },
    #[error("serial: {0}")]
    Io(#[from] io::Error),
}

impl CmdError {
    /// The error class name of a packing failure (`OverflowError`, or that of the integer conversion).
    pub fn class(&self) -> &str {
        match self {
            CmdError::ArgumentCount | CmdError::ReceivedArgumentCount => "ValueError",
            CmdError::Overflow { .. } => "OverflowError",
            CmdError::Native(error) => error.split_once(": ").map_or(error.as_str(), |(class, _)| class),
            CmdError::NotAscii => "UnicodeEncodeError",
            CmdError::Incomplete(_) => "EOFError",
            _ => "OSError",
        }
    }
}

fn escape_into(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if matches!(b, FIELD_SEPARATOR | COMMAND_SEPARATOR | ESCAPE | 0) {
            out.push(ESCAPE);
        }
        out.push(b);
    }
}

/// Packs a byte, 16-bit or 32-bit field: a value that is not an integer is converted first,
/// then the field's range is checked on the
/// arbitrary-precision result; out of range is an overflow error with its text.
fn integer(arg: &Arg, min: i64, max: i64, field: &'static str) -> Result<i64, CmdError> {
    use openrigcue_model::native_value::LegacyValue;
    let integer = match arg {
        Arg::Number(v) => LegacyValue::Float(*v).py_integer(),
        Arg::Text(text) => LegacyValue::Str(text.clone()).py_integer(),
        Arg::Native(value) => match value.resolved() {
            // Already an integer: no conversion.
            LegacyValue::Int(v) => Ok(v.into()),
            LegacyValue::BigInt(v) => Ok(v),
            other => other.py_integer(),
        },
    }
    .map_err(CmdError::Native)?;
    match i64::try_from(&integer) {
        Ok(value) if (min..=max).contains(&value) => Ok(value),
        // The message holds the value's decimal text: the decimal digit limit applies while formatting it.
        _ => Err(match openrigcue_model::python_repr::integer_decimal(&integer) {
            Ok(value) => CmdError::Overflow { value, field },
            Err(error) => CmdError::Native(error),
        }),
    }
}

fn encode_field(field: Field, arg: &Arg) -> Result<Vec<u8>, CmdError> {
    Ok(match field {
        Field::Byte => vec![integer(arg, 0, 255, "byte")? as u8],
        Field::Int16 => (integer(arg, i16::MIN.into(), i16::MAX.into(), "int")? as i16).to_le_bytes().to_vec(),
        Field::Int32 => (integer(arg, i32::MIN.into(), i32::MAX.into(), "long")? as i32).to_le_bytes().to_vec(),
        Field::Text => match arg {
            Arg::Text(s) if s.is_ascii() => s.as_bytes().to_vec(),
            Arg::Text(_) => return Err(CmdError::NotAscii),
            Arg::Number(v) => format_python_number(*v).into_bytes(),
            // Text field: bytes pass unchanged; anything else is its text form, ASCII-encoded.
            Arg::Native(value) => match value.resolved() {
                openrigcue_model::native_value::LegacyValue::Bytes(bytes) => bytes,
                _ => {
                    let text = openrigcue_model::python_repr::str_native(value).map_err(CmdError::Native)?;
                    let points = text.codepoints();
                    if points.iter().any(|&point| point > 0x7f) {
                        return Err(CmdError::NotAscii);
                    }
                    points.iter().map(|&point| point as u8).collect()
                }
            },
        },
    })
}

/// The text form of the numbers this crate sends as text.
fn format_python_number(v: f64) -> String {
    if v.is_nan() {
        "nan".into()
    } else if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Encode `table[index]` with its arguments.
pub fn encode(table: &[CommandSpec], index: usize, args: &[Arg]) -> Result<Vec<u8>, CmdError> {
    let spec = table[index];
    if !args.is_empty() && args.len() != spec.fields.len() {
        return Err(CmdError::ArgumentCount);
    }
    let mut frame = index.to_string().into_bytes();
    for (field, arg) in spec.fields.iter().zip(args) {
        frame.push(FIELD_SEPARATOR);
        escape_into(&mut frame, &encode_field(*field, arg)?);
    }
    frame.push(COMMAND_SEPARATOR);
    Ok(frame)
}

/// Encode and write a command.
pub fn send(link: &mut dyn SerialLink, table: &[CommandSpec], index: usize, args: &[Arg]) -> Result<(), CmdError> {
    let frame = encode(table, index, args)?;
    link.write(&frame)?;
    Ok(())
}

fn decode_field(field: Field, raw: &[u8]) -> Result<Value, CmdError> {
    let bad = || CmdError::BadField { field, got: raw.len() };
    Ok(match field {
        Field::Byte => Value::Int(i64::from(*raw.first().filter(|_| raw.len() == 1).ok_or_else(bad)?)),
        Field::Int16 => Value::Int(i16::from_le_bytes(raw.try_into().map_err(|_| bad())?).into()),
        Field::Int32 => Value::Int(i32::from_le_bytes(raw.try_into().map_err(|_| bad())?).into()),
        Field::Text => {
            if !raw.is_ascii() {
                return Err(CmdError::NotAscii);
            }
            let text = String::from_utf8_lossy(raw);
            Value::Text(text.trim_matches('\0').trim_matches(is_python_whitespace).to_string())
        }
    })
}

/// Whitespace for ASCII text, which includes the information separators 0x1c-0x1f.
fn is_python_whitespace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// Read one command; `Ok(None)` when nothing (or only whitespace) arrived before the read timeout.
///
/// Bytes are read one at a time until `;`, honouring escapes; a read
/// timeout in the middle of a message is an error.
pub fn receive(link: &mut dyn SerialLink, table: &[CommandSpec]) -> Result<Option<Message>, CmdError> {
    let mut fields: Vec<Vec<u8>> = vec![Vec::new()];
    let mut raw: Vec<u8> = Vec::new();
    let mut escaped = false;
    let mut terminated = false;
    loop {
        let byte = link.read_byte()?;
        if let Some(b) = byte {
            raw.push(b);
        }
        let current = fields.last_mut().expect("at least one field");
        if escaped {
            // An escaped non-special byte keeps its escape; a timeout while escaped keeps reading.
            match byte {
                Some(b) if matches!(b, FIELD_SEPARATOR | COMMAND_SEPARATOR | ESCAPE | 0) => current.push(b),
                Some(b) => current.extend_from_slice(&[ESCAPE, b]),
                None => current.push(ESCAPE),
            }
            escaped = false;
            continue;
        }
        match byte {
            Some(ESCAPE) => escaped = true,
            Some(FIELD_SEPARATOR) => fields.push(Vec::new()),
            Some(COMMAND_SEPARATOR) => {
                terminated = true;
                break;
            }
            None => break,
            Some(b) => current.push(b),
        }
    }
    if fields.len() == 1 && fields[0].is_empty() {
        return Ok(None);
    }
    if !terminated {
        if raw.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        return Err(CmdError::Incomplete(raw));
    }
    decode_message(table, &fields)
}

fn decode_message(table: &[CommandSpec], fields: &[Vec<u8>]) -> Result<Option<Message>, CmdError> {
    let head = std::str::from_utf8(&fields[0]).map_err(|_| CmdError::NotAscii)?;
    let args = &fields[1..];
    // The first field, trimmed, is the command index; text that is not a number is the unknown command,
    // an index the table does not have is an error.
    let index = match super::numeric::python_int_checked(head) {
        Ok(index) => index,
        Err(super::numeric::PyIntError::Invalid) => {
            return Ok(Some(Message { command: None, args: args.iter().map(|a| Value::Raw(a.clone())).collect() }));
        }
        Err(super::numeric::PyIntError::OutOfRange) => return Err(CmdError::UnknownIndex(i64::MAX)),
    };
    let spec = usize::try_from(index).ok().and_then(|i| table.get(i)).ok_or(CmdError::UnknownIndex(index))?;
    if !args.is_empty() && args.len() != spec.fields.len() {
        return Err(CmdError::ReceivedArgumentCount);
    }
    let decoded = spec.fields.iter().zip(args).map(|(f, raw)| decode_field(*f, raw)).collect::<Result<_, _>>()?;
    Ok(Some(Message { command: Some(index as usize), args: decoded }))
}

#[cfg(test)]
mod tests {
    use super::super::link::fakes::FakeLink;
    use super::*;

    const TABLE: &[CommandSpec] = &[
        CommandSpec { name: "who_are_you", fields: &[Field::Text] },
        CommandSpec {
            name: "i_am",
            fields: &[Field::Text, Field::Byte, Field::Byte, Field::Byte, Field::Byte, Field::Text],
        },
        CommandSpec { name: "send_stream", fields: &[Field::Int16, Field::Int32] },
        CommandSpec { name: "ping", fields: &[] },
    ];

    #[test]
    fn binary_fields_convert_decimal_text_before_packing() {
        let expected = encode(TABLE, 2, &[Arg::Number(300.0), Arg::Number(-1.0)]).unwrap();
        assert_eq!(encode(TABLE, 2, &[Arg::Text(" +٣_٠٠ ".into()), Arg::Text("-1".into())]).unwrap(), expected);
        assert_eq!(
            encode(TABLE, 2, &[Arg::Text(format!("{}300", "0".repeat(4297))), Arg::Text("-1".into())]).unwrap(),
            expected
        );
        let error =
            encode(TABLE, 2, &[Arg::Text(format!("{}300", "0".repeat(4298))), Arg::Text("-1".into())]).unwrap_err();
        assert_eq!(error.class(), "ValueError");
        assert!(error.to_string().starts_with("Exceeds the limit (4300 digits)"), "{error}");
        let error = encode(TABLE, 2, &[Arg::Text("99999999999999999999".into()), Arg::Text("-1".into())]).unwrap_err();
        assert_eq!(
            (error.class(), error.to_string().as_str()),
            ("OverflowError", "Value 99999999999999999999 exceeds the size of the board's int.")
        );
        let mut link = FakeLink::new();
        assert!(send(&mut link, TABLE, 2, &[Arg::Text("300".into()), Arg::Text("1.5".into())]).is_err());
        assert!(link.take_written().is_empty(), "an invalid later field cannot flush the valid prefix");
    }

    /// Send framing and escaping.
    #[test]
    fn send_frames_binary_fields_with_escapes() {
        let frame = encode(TABLE, 2, &[Arg::Number(300.9), Arg::Number(-1.0)]).unwrap();
        // 300 = 0x012C little-endian 2C 01; ',' (0x2C) inside a field is escaped.
        assert_eq!(frame, b"2,/,\x01,\xff\xff\xff\xff;".to_vec());
        assert_eq!(encode(TABLE, 3, &[]).unwrap(), b"3;".to_vec());
        assert_eq!(encode(TABLE, 0, &[Arg::Text("a;b".into())]).unwrap(), b"0,a/;b;".to_vec());
    }

    /// Bounds of the 16-bit and byte fields.
    #[test]
    fn send_rejects_overflow_and_count_mismatch() {
        let error = encode(TABLE, 2, &[Arg::Number(40000.9), Arg::Number(0.0)]).unwrap_err();
        assert_eq!(error.to_string(), "Value 40000 exceeds the size of the board's int.");
        let error = encode(TABLE, 2, &[Arg::Number(1.0), Arg::Number(2_147_483_648.0)]).unwrap_err();
        assert_eq!(error.to_string(), "Value 2147483648 exceeds the size of the board's long.");
        let error = encode(
            TABLE,
            1,
            &[
                Arg::Text("x".into()),
                Arg::Number(256.0),
                Arg::Number(0.0),
                Arg::Number(0.0),
                Arg::Number(0.0),
                Arg::Text("y".into()),
            ],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Value 256 exceeds the size of the board's byte.");
        let error = encode(TABLE, 2, &[Arg::Number(1.0)]).unwrap_err();
        assert_eq!(error.to_string(), "Number of argument formats must match the number of arguments.");
        let error = encode(TABLE, 1, &vec![Arg::Number(f64::NAN); 6]).unwrap_err();
        assert_eq!((error.class(), error.to_string().as_str()), ("ValueError", "cannot convert float NaN to integer"));
        let error = encode(TABLE, 2, &[Arg::Number(f64::INFINITY), Arg::Number(0.0)]).unwrap_err();
        assert_eq!(
            (error.class(), error.to_string().as_str()),
            ("OverflowError", "cannot convert float infinity to integer")
        );
    }

    /// Integer conversion of retained raw values, then the range check on the full integer.
    #[test]
    fn native_arguments_coerce_and_range_check_at_the_packer() {
        use openrigcue_model::native_value::{BigInt, LegacyValue};
        let native = |value: LegacyValue| Arg::Native(value);
        let wide = BigInt::from(1u8) << 70usize;
        let error = encode(TABLE, 2, &[native(LegacyValue::BigInt(wide.clone())), Arg::Number(0.0)]).unwrap_err();
        assert_eq!(error.to_string(), format!("Value {wide} exceeds the size of the board's int."));
        let frame =
            encode(TABLE, 2, &[native(LegacyValue::Bool(true)), native(LegacyValue::Str(" -2 ".into()))]).unwrap();
        assert_eq!(frame, b"2,\x01/\x00,\xfe\xff\xff\xff;".to_vec());
        let error = encode(TABLE, 2, &[native(LegacyValue::None), Arg::Number(0.0)]).unwrap_err();
        assert_eq!(error.class(), "TypeError");
        assert_eq!(
            error.to_string(),
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
        );
        let error =
            encode(TABLE, 2, &[native(LegacyValue::Float(1.5)), native(LegacyValue::Str("1.5".into()))]).unwrap_err();
        assert_eq!(error.to_string(), "invalid literal for int() with base 10: '1.5'");
        // The overflow message holds the integer's digits: beyond 4300 digits that formatting fails instead.
        let huge = BigInt::from(10u8).pow(5000u32);
        let error = encode(TABLE, 2, &[native(LegacyValue::BigInt(huge)), Arg::Number(0.0)]).unwrap_err();
        assert_eq!(error.class(), "ValueError");
        assert!(error.to_string().starts_with("Exceeds the limit (4300 digits)"), "{error}");
        // Text field: bytes pass unchanged; other values are their text form, ASCII-encoded.
        let frame = encode(TABLE, 0, &[native(LegacyValue::Bytes(b"a;\xff".to_vec()))]).unwrap();
        assert_eq!(frame, b"0,a/;\xff;".to_vec());
        assert_eq!(encode(TABLE, 0, &[native(LegacyValue::Int(-7))]).unwrap(), b"0,-7;".to_vec());
        assert!(matches!(encode(TABLE, 0, &[native(LegacyValue::Str("é".into()))]), Err(CmdError::NotAscii)));
    }

    /// Receiving and decoding a command.
    #[test]
    fn receive_decodes_escaped_binary_fields() {
        let link = FakeLink::new();
        link.push_incoming(b"1,ab12 \0,\x07,/;,\x05,\x01, SN1;");
        let mut l = link.clone();
        let msg = receive(&mut l, TABLE).unwrap().unwrap();
        assert!(msg.is(1));
        assert_eq!(
            msg.args,
            vec![
                Value::Text("ab12".into()),
                Value::Int(7),
                Value::Int(59),
                Value::Int(5),
                Value::Int(1),
                Value::Text("SN1".into())
            ]
        );
    }

    /// Receive: timeout before anything -> None; mid-message -> end-of-input error; garbage index -> unknown command
    #[test]
    fn receive_timeout_and_error_paths() {
        let mut empty = FakeLink::new();
        assert!(receive(&mut empty, TABLE).unwrap().is_none());
        let mut blank = FakeLink::new();
        blank.push_incoming(b"\r\n");
        assert!(receive(&mut blank, TABLE).unwrap().is_none());
        let mut partial = FakeLink::new();
        partial.push_incoming(b"2,\x01");
        assert!(matches!(receive(&mut partial, TABLE), Err(CmdError::Incomplete(_))));
        let mut unknown = FakeLink::new();
        unknown.push_incoming(b"x,1;");
        assert_eq!(receive(&mut unknown, TABLE).unwrap().unwrap().command, None);
        let mut out_of_table = FakeLink::new();
        out_of_table.push_incoming(b"9;");
        assert!(matches!(receive(&mut out_of_table, TABLE), Err(CmdError::UnknownIndex(9))));
        let mut wrong_count = FakeLink::new();
        wrong_count.push_incoming(b"1,x;");
        assert!(matches!(receive(&mut wrong_count, TABLE), Err(CmdError::ReceivedArgumentCount)));
        let mut binary_text = FakeLink::new();
        binary_text.push_incoming(b"0,\xc3\xa9;");
        assert!(matches!(receive(&mut binary_text, TABLE), Err(CmdError::NotAscii)));
    }
}

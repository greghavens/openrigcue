//! SimConnect TCP framing and the messages the listeners use.
//!
//! Client → simulator: a header of four `u32` (size, protocol, `code | 0xF0000000`, counter) and the body. Simulator →
//! client: a header of three `u32` (size, protocol, code) and a body decoded member by member. All little-endian; strings are
//! fixed-size, NUL-padded, Latin-1.

use openrigcue_model::native_value::py_float_repr;
use openrigcue_model::python_repr::repr_str;

use crate::printed::bytes_repr;

/// `SIMCONNECT_DATATYPE` values used by the listeners' data definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Float32 = 3,
    String32 = 6,
    String256 = 9,
}

impl DataType {
    /// Bytes one value of this type occupies in a data block.
    pub fn size(self) -> usize {
        match self {
            DataType::Float32 => 4,
            DataType::String32 => 32,
            DataType::String256 => 256,
        }
    }
}

/// `SIMCONNECT_PERIOD_SIM_FRAME` and `SIMCONNECT_DATA_REQUEST_FLAG_CHANGED`.
pub const PERIOD_SIM_FRAME: u32 = 3;
pub const FLAG_CHANGED: u32 = 1;
/// `SIMCONNECT_OBJECT_ID_USER`.
pub const OBJECT_ID_USER: u32 = 0;
/// `datumID` of an untagged definition entry.
pub const UNUSED_DATUM_ID: u32 = 0xFFFF_FFFF;

/// The client messages the listeners send.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Open {
        app_name: &'static str,
        sim_id: &'static str,
        version: [u32; 2],
        build: [u32; 2],
    },
    MapClientEventToSimEvent {
        event_id: u32,
        event_name: &'static str,
    },
    AddClientEventToNotificationGroup {
        group_id: u32,
        event_id: u32,
        maskable: u32,
    },
    SetNotificationGroupPriority {
        group_id: u32,
        priority: u32,
    },
    AddToDataDefinition {
        definition_id: u32,
        datum_name: &'static str,
        units_name: &'static str,
        data_type: DataType,
        epsilon: f32,
        datum_id: u32,
    },
    RequestDataOnSimObject {
        request_id: u32,
        definition_id: u32,
        object_id: u32,
        period: u32,
        flags: u32,
        origin: u32,
        interval: u32,
        limit: u32,
    },
    SubscribeToSystemEvent {
        client_event_id: u32,
        event_name: &'static str,
    },
    RequestSystemState {
        request_id: u32,
        state_name: &'static str,
    },
}

impl ClientMessage {
    /// The message code.
    fn code(&self) -> u32 {
        match self {
            ClientMessage::Open { .. } => 1,
            ClientMessage::MapClientEventToSimEvent { .. } => 4,
            ClientMessage::AddClientEventToNotificationGroup { .. } => 7,
            ClientMessage::SetNotificationGroupPriority { .. } => 9,
            ClientMessage::AddToDataDefinition { .. } => 12,
            ClientMessage::RequestDataOnSimObject { .. } => 14,
            ClientMessage::SubscribeToSystemEvent { .. } => 23,
            ClientMessage::RequestSystemState { .. } => 53,
        }
    }

    /// Encodes a client message.
    pub fn encode(&self, protocol: u32, counter: u32) -> Vec<u8> {
        let mut body = Vec::new();
        let put =
            |body: &mut Vec<u8>, values: &[u32]| values.iter().for_each(|v| body.extend_from_slice(&v.to_le_bytes()));
        match *self {
            ClientMessage::Open { app_name, sim_id, version, build } => {
                put_text(&mut body, app_name, 256);
                put(&mut body, &[0]);
                body.push(0);
                put_text(&mut body, sim_id, 3);
                put(&mut body, &version);
                put(&mut body, &build);
            }
            ClientMessage::MapClientEventToSimEvent { event_id, event_name } => {
                put(&mut body, &[event_id]);
                put_text(&mut body, event_name, 256);
            }
            ClientMessage::AddClientEventToNotificationGroup { group_id, event_id, maskable } => {
                put(&mut body, &[group_id, event_id, maskable]);
            }
            ClientMessage::SetNotificationGroupPriority { group_id, priority } => put(&mut body, &[group_id, priority]),
            ClientMessage::AddToDataDefinition {
                definition_id,
                datum_name,
                units_name,
                data_type,
                epsilon,
                datum_id,
            } => {
                put(&mut body, &[definition_id]);
                put_text(&mut body, datum_name, 256);
                put_text(&mut body, units_name, 256);
                put(&mut body, &[data_type as u32]);
                body.extend_from_slice(&epsilon.to_le_bytes());
                put(&mut body, &[datum_id]);
            }
            ClientMessage::RequestDataOnSimObject {
                request_id,
                definition_id,
                object_id,
                period,
                flags,
                origin,
                interval,
                limit,
            } => {
                put(&mut body, &[request_id, definition_id, object_id, period, flags, origin, interval, limit]);
            }
            ClientMessage::SubscribeToSystemEvent { client_event_id, event_name } => {
                put(&mut body, &[client_event_id]);
                put_text(&mut body, event_name, 256);
            }
            ClientMessage::RequestSystemState { request_id, state_name } => {
                put(&mut body, &[request_id]);
                put_text(&mut body, state_name, 256);
            }
        }
        let size = u32::try_from(16 + body.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(16 + body.len());
        for word in [size, protocol, self.code() | 0xF000_0000, counter] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&body);
        out
    }
}

/// Latin-1, NUL-padded to `len` (the names used here are ASCII and shorter than their field).
fn put_text(body: &mut Vec<u8>, text: &str, len: usize) {
    let mut bytes: Vec<u8> = text.chars().map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).take(len).collect();
    bytes.resize(len, 0);
    body.extend_from_slice(&bytes);
}

/// The simulator messages the listeners understand.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Open {
        app_name: String,
    },
    Event {
        group_id: u32,
        event_id: u32,
        data: i32,
    },
    /// `SSimObjectData` (code 8): the requested data block.
    SimObjectData {
        request_id: u32,
        define_id: u32,
        data: Vec<u8>,
    },
    SystemState {
        request_id: u32,
        integer: u32,
        string: String,
    },
    /// A message class the listeners have no handler for (exception, quit, object data by type,
    /// joystick device info); `repr` is the message's printed member list.
    Unhandled {
        repr: String,
    },
}

/// A buffer the parser rejects; the text is the printed error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// No server message class has this code.
    #[error("('s', {0})")]
    UnknownCode(u32),
    /// A failed size check, or a class without members whose message has bytes left.
    #[error("{0}")]
    Layout(String),
    /// The message does not encode back to its bytes.
    #[error("ENCODING FAILURE")]
    NotCanonical,
}

/// One member of a server message layout.
#[derive(Debug, Clone, Copy)]
enum Member {
    U32,
    I32,
    F32,
    Text(usize),
    /// An array of `n` `u32`.
    U32s(usize),
    /// An array of joystick device records: the count is the preceding member.
    Joysticks,
    /// The rest of the message: whatever the header size leaves.
    Remaining,
}

/// The members of one server class, by name.
type Members = &'static [(&'static str, Member)];

/// Class name and members of a server message code.
fn layout(code: u32) -> Option<(&'static str, Members)> {
    use Member::*;
    const OBJECT_DATA: Members = &[
        ("requestID", U32),
        ("objectID", U32),
        ("definitionID", U32),
        ("flags", U32),
        ("entryNumber", U32),
        ("outOf", U32),
        ("defineCount", U32),
        ("data", Remaining),
    ];
    Some(match code {
        1 => ("SException", &[("exception", U32), ("sendID", U32), ("index", U32)]),
        2 => (
            "SOpen",
            &[
                ("appName", Text(256)),
                ("appVer", U32s(2)),
                ("appBuild", U32s(2)),
                ("scVer", U32s(2)),
                ("scBuild", U32s(2)),
                ("_ignore", U32),
                ("_ignore2", U32),
            ],
        ),
        3 => ("SQuit", &[]),
        4 => ("SEvent", &[("groupID", U32), ("eventID", U32), ("data", I32), ("flags", U32)]),
        8 => ("SSimObjectData", OBJECT_DATA),
        9 => ("SSimObjectDataByType", OBJECT_DATA),
        15 => {
            ("SSystemState", &[("requestID", U32), ("dataInteger", U32), ("dataFloat", F32), ("dataString", Text(260))])
        }
        64 => ("SJoystickDeviceInfo", &[("requestID", U32), ("count", U32), ("joysticks", Joysticks)]),
        _ => return None,
    })
}

/// A decoded member value.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    Word(u32),
    Int(i32),
    /// The bits of a float member.
    Float(u32),
    Text(String),
    Words(Vec<u32>),
    /// Name and number of each joystick device record.
    Joysticks(Vec<(String, u32)>),
    /// The rest of the message: empty until the header size leaves bytes for it, then those bytes.
    Remaining(Option<Vec<u8>>),
}

impl Value {
    /// The printed form of the value.
    fn repr(&self) -> String {
        match self {
            Value::Word(word) => word.to_string(),
            Value::Int(int) => int.to_string(),
            Value::Float(bits) => py_float_repr(f64::from(f32::from_bits(*bits))),
            Value::Text(text) => repr_str(text),
            Value::Words(words) => {
                format!("[{}]", words.iter().map(u32::to_string).collect::<Vec<_>>().join(", "))
            }
            Value::Joysticks(entries) => {
                let entries: Vec<String> = entries
                    .iter()
                    .map(|(name, number)| format!("<JoystickDeviceInfo: name: {}, number: {number}>", repr_str(name)))
                    .collect();
                format!("[{}]", entries.join(", "))
            }
            Value::Remaining(None) => "b''".into(),
            Value::Remaining(Some(bytes)) => format!("bytearray({})", bytes_repr(bytes)),
        }
    }
}

/// The printed form of a message: `<SEvent: groupID: 1, eventID: 2, data: 3, flags: 4>`.
fn struct_repr(class: &str, members: Members, values: &[Value]) -> String {
    let items: Vec<String> =
        members.iter().zip(values).map(|((name, _), value)| format!("{name}: {}", value.repr())).collect();
    format!("<{class}: {}>", items.join(", "))
}

/// Size of the server header (three `u32`).
const SERVER_HEADER: usize = 12;

/// Parses one server message and checks that it encodes back to the same bytes: `Ok(None)` until a
/// whole message is buffered, then the message and the bytes it used. An error carries the printed text of the
/// failure.
///
/// Members are read against the whole buffer (the bound is the buffer length, not the header size, so a message
/// whose header size is shorter than its layout reads into the next message and then fails its size check).
pub fn decode_server(buffer: &[u8]) -> Result<Option<(ServerMessage, usize)>, WireError> {
    if buffer.len() < SERVER_HEADER {
        return Ok(None);
    }
    let word = |at: usize| u32::from_le_bytes([buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]]);
    let (size, raw_code) = (word(0) as usize, word(8));
    if buffer.len() < size {
        return Ok(None);
    }
    let code = raw_code & 0x0FFF_FFFF;
    let (class, members) = layout(code).ok_or(WireError::UnknownCode(code))?;
    let mut offset = SERVER_HEADER;
    let mut values = Vec::with_capacity(members.len());
    // The message as it encodes back to bytes.
    let mut encoded = buffer[..8].to_vec();
    encoded.extend_from_slice(&code.to_le_bytes());
    let mut joystick_count = 0usize;
    for (_, member) in members {
        let need = match *member {
            Member::U32 | Member::I32 | Member::F32 => 4,
            Member::Text(len) => len,
            Member::U32s(n) => 4 * n,
            Member::Joysticks => joystick_count.saturating_mul(132),
            Member::Remaining => 0,
        };
        // The member must fit in the buffer; this failure has no text.
        let field =
            offset.checked_add(need).and_then(|end| buffer.get(offset..end)).ok_or(WireError::Layout(String::new()))?;
        match *member {
            Member::U32 => {
                joystick_count = word(offset) as usize;
                values.push(Value::Word(word(offset)));
                encoded.extend_from_slice(field);
            }
            Member::I32 => {
                values.push(Value::Int(word(offset) as i32));
                encoded.extend_from_slice(field);
            }
            Member::F32 => {
                let bits = word(offset);
                values.push(Value::Float(bits));
                // The float is widened to a double and narrowed back: every value comes back as it was except a
                // signalling NaN, which the two conversions return quiet.
                let repacked = if f32::from_bits(bits).is_nan() { bits | 0x0040_0000 } else { bits };
                encoded.extend_from_slice(&repacked.to_le_bytes());
            }
            Member::Text(_) => values.push(Value::Text(text(field, &mut encoded))),
            Member::U32s(_) => {
                let words = field.as_chunks::<4>().0.iter().map(|word| u32::from_le_bytes(*word)).collect();
                values.push(Value::Words(words));
                encoded.extend_from_slice(field);
            }
            Member::Joysticks => {
                let entries = field
                    .as_chunks::<132>()
                    .0
                    .iter()
                    .map(|entry| {
                        let name = text(&entry[..128], &mut encoded);
                        encoded.extend_from_slice(&entry[128..]);
                        (name, u32::from_le_bytes([entry[128], entry[129], entry[130], entry[131]]))
                    })
                    .collect();
                values.push(Value::Joysticks(entries));
            }
            Member::Remaining => values.push(Value::Remaining(None)),
        }
        offset += need;
    }
    let consumed = offset - SERVER_HEADER;
    // The members must not use more than the header size; the failure text lists the sizes and the message.
    let Some(remaining) = size.checked_sub(offset) else {
        let missing = offset - size;
        let message = struct_repr(class, members, &values);
        return Err(WireError::Layout(format!("(-{missing}, {message}, {size}, {SERVER_HEADER}, {consumed})")));
    };
    if remaining > 0 {
        if members.is_empty() {
            // A class without members has no last member.
            return Err(WireError::Layout("list index out of range".into()));
        }
        if let Some(Value::Remaining(data)) = values.last_mut() {
            *data = Some(buffer[offset..size].to_vec());
            encoded.extend_from_slice(&buffer[offset..size]);
        } else {
            let message = struct_repr(class, members, &values);
            return Err(WireError::Layout(format!(
                "{remaining} bytes remain for {message}, expected a Remaining parameter"
            )));
        }
    }
    let raw = &buffer[..size];
    if encoded != raw {
        // The message, its raw bytes and its re-encoded bytes are logged.
        tracing::info!("MSG: {}", struct_repr(class, members, &values));
        tracing::info!("RAW: bytearray({})", bytes_repr(raw));
        tracing::info!("ENC: {}", bytes_repr(&encoded));
        return Err(WireError::NotCanonical);
    }
    Ok(Some((assemble(code, struct_repr(class, members, &values), values), size)))
}

/// A fixed string field: the Latin-1 text up to the first NUL. `encoded` receives the field as
/// that text encodes back (every byte after the text is NUL).
fn text(field: &[u8], encoded: &mut Vec<u8>) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    encoded.extend_from_slice(&field[..end]);
    encoded.resize(encoded.len() + field.len() - end, 0);
    field[..end].iter().map(|&b| char::from(b)).collect()
}

fn assemble(code: u32, repr: String, values: Vec<Value>) -> ServerMessage {
    let mut values = values.into_iter();
    let mut word = || match values.next() {
        Some(Value::Word(word)) => word,
        Some(Value::Int(int)) => int as u32,
        _ => 0,
    };
    match code {
        4 => ServerMessage::Event { group_id: word(), event_id: word(), data: word() as i32 },
        8 => {
            let (request_id, _, define_id) = (word(), word(), word());
            let data = match values.last() {
                Some(Value::Remaining(Some(data))) => data,
                _ => Vec::new(),
            };
            ServerMessage::SimObjectData { request_id, define_id, data }
        }
        15 => {
            let (request_id, integer) = (word(), word());
            let string = match values.nth(1) {
                Some(Value::Text(text)) => text,
                _ => String::new(),
            };
            ServerMessage::SystemState { request_id, integer, string }
        }
        2 => match values.next() {
            Some(Value::Text(app_name)) => ServerMessage::Open { app_name },
            _ => ServerMessage::Open { app_name: String::new() },
        },
        _ => ServerMessage::Unhandled { repr },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A server message as the simulator frames it.
    pub(crate) fn server_frame(code: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for word in [12 + body.len() as u32, 4, code] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(body);
        out
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// The open message: a 256-byte string, a u32, a byte, a 3-byte string and four u32 after the 16-byte client header.
    #[test]
    fn open_encoding() {
        let bytes =
            ClientMessage::Open { app_name: "SRS", sim_id: "XSF", version: [10, 0], build: [62615, 0] }.encode(4, 7);
        assert_eq!(bytes.len(), 16 + 256 + 4 + 1 + 3 + 8 + 8);
        assert_eq!(
            bytes[..16],
            [&296u32.to_le_bytes()[..], &4u32.to_le_bytes(), &0xF000_0001u32.to_le_bytes(), &7u32.to_le_bytes()]
                .concat()[..]
        );
        assert_eq!(&bytes[16..19], b"SRS");
        assert_eq!(bytes[19], 0);
        assert_eq!(&bytes[16 + 256 + 5..16 + 256 + 8], b"XSF");
        assert_eq!(&bytes[16 + 264..16 + 268], &10u32.to_le_bytes());
        assert_eq!(&bytes[16 + 272..16 + 276], &62615u32.to_le_bytes());
    }

    /// The layouts of the add-to-data-definition and request-data-on-sim-object messages.
    #[test]
    fn data_definition_encoding() {
        let add = ClientMessage::AddToDataDefinition {
            definition_id: 1,
            datum_name: "PLANE PITCH DEGREES",
            units_name: "Degrees",
            data_type: DataType::Float32,
            epsilon: 0.0,
            datum_id: UNUSED_DATUM_ID,
        }
        .encode(4, 0);
        assert_eq!(add.len(), 16 + 4 + 256 + 256 + 4 + 4 + 4);
        assert_eq!(&add[8..12], &0xF000_000Cu32.to_le_bytes());
        assert_eq!(&add[16 + 4 + 512..16 + 4 + 516], &3u32.to_le_bytes());
        assert_eq!(&add[add.len() - 4..], &[0xFF; 4]);
        let request = ClientMessage::RequestDataOnSimObject {
            request_id: 1,
            definition_id: 1,
            object_id: 0,
            period: 3,
            flags: 1,
            origin: 0,
            interval: 0,
            limit: 0,
        }
        .encode(29, 3);
        assert_eq!(&request[16..], &words(&[1, 1, 0, 3, 1, 0, 0, 0])[..]);
        assert_eq!(&request[4..8], &29u32.to_le_bytes());
    }

    /// Parsing waits for whole messages, takes the data block from the header size.
    #[test]
    fn server_decoding() {
        let mut body = words(&[1, 0, 1, 0, 0, 1, 2]);
        body.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let frame = server_frame(8, &body);
        assert_eq!(decode_server(&frame[..20]), Ok(None));
        let (message, used) = decode_server(&frame).unwrap().unwrap();
        assert_eq!(used, frame.len());
        assert_eq!(
            message,
            ServerMessage::SimObjectData { request_id: 1, define_id: 1, data: vec![1, 2, 3, 4, 5, 6, 7, 8] }
        );
        let quit = server_frame(3, &[]);
        assert_eq!(decode_server(&quit).unwrap().unwrap().0, ServerMessage::Unhandled { repr: "<SQuit: >".into() });
    }

    /// Failure paths: unknown code, a body longer than a fixed layout, a header size
    /// shorter than the layout (the 12-byte SIMCONNECT_RECV_EVENT body vs the 16-byte SEvent layout), a string with
    /// bytes after its NUL.
    #[test]
    fn server_decoding_failures() {
        assert_eq!(decode_server(&server_frame(6, &[0; 8])), Err(WireError::UnknownCode(6)));
        let failure = |frame: &[u8]| decode_server(frame).unwrap_err().to_string();
        assert_eq!(failure(&server_frame(6, &[0; 8])), "('s', 6)", "KeyError(('s', 6))");
        assert_eq!(
            failure(&server_frame(1, &words(&[1, 2, 3, 4]))),
            "4 bytes remain for <SException: exception: 1, sendID: 2, index: 3>, expected a Remaining parameter"
        );
        assert_eq!(failure(&server_frame(3, &[0; 4])), "list index out of range", "SQuit has no last member");
        let mut event = server_frame(4, &words(&[0, 3, 0]));
        event.extend_from_slice(&server_frame(3, &[]));
        assert_eq!(failure(&event), "(-4, <SEvent: groupID: 0, eventID: 3, data: 0, flags: 12>, 24, 12, 16)");
        // Without a following message the member read itself fails, with no text.
        assert_eq!(decode_server(&server_frame(4, &words(&[0, 3, 0]))), Err(WireError::Layout(String::new())));
        let mut name = [0u8; 256];
        name[..3].copy_from_slice(b"FSX");
        name[10] = b'x';
        let mut open = name.to_vec();
        open.extend_from_slice(&[0; 40]);
        assert_eq!(decode_server(&server_frame(2, &open)), Err(WireError::NotCanonical));
        let mut high_bits = server_frame(3, &[]);
        high_bits[11] = 0x10;
        assert_eq!(decode_server(&high_bits), Err(WireError::NotCanonical));
        assert_eq!(WireError::NotCanonical.to_string(), "ENCODING FAILURE");
    }

    /// The "no handler" line prints the message with each member as `name: value`.
    #[test]
    fn messages_without_handler_keep_their_python_repr() {
        let repr = |frame: &[u8]| match decode_server(frame).unwrap().unwrap().0 {
            ServerMessage::Unhandled { repr } => repr,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            repr(&server_frame(1, &words(&[7, 21, 0xFFFF_FFFF]))),
            "<SException: exception: 7, sendID: 21, index: 4294967295>"
        );
        let block = words(&[1, 0, 1, 0, 0, 1, 2]);
        assert_eq!(
            repr(&server_frame(9, &block)),
            "<SSimObjectDataByType: requestID: 1, objectID: 0, definitionID: 1, flags: 0, entryNumber: 0, outOf: 1, \
             defineCount: 2, data: b''>"
        );
        let with_data = [block, b"a\x00\xff'".to_vec()].concat();
        assert_eq!(
            repr(&server_frame(9, &with_data)),
            "<SSimObjectDataByType: requestID: 1, objectID: 0, definitionID: 1, flags: 0, entryNumber: 0, outOf: 1, \
             defineCount: 2, data: bytearray(b\"a\\x00\\xff'\")>"
        );
        let mut joysticks = words(&[5, 1]);
        let mut name = [0u8; 128];
        name[..5].copy_from_slice(b"Stick");
        joysticks.extend_from_slice(&name);
        joysticks.extend_from_slice(&3u32.to_le_bytes());
        assert_eq!(
            repr(&server_frame(64, &joysticks)),
            "<SJoystickDeviceInfo: requestID: 5, count: 1, joysticks: [<JoystickDeviceInfo: name: 'Stick', number: 3>]>"
        );
    }

    /// A float decoded and encoded again: a signalling NaN does not come back as its own bytes, so the
    /// message fails the re-encoding check; a quiet NaN and every other value pass.
    #[test]
    fn a_signalling_nan_fails_the_encoding_check() {
        let state = |bits: u32| {
            let mut body = words(&[0, 1, bits]);
            body.extend_from_slice(&[0u8; 260]);
            decode_server(&server_frame(15, &body))
        };
        assert_eq!(state(0x7FA0_0000), Err(WireError::NotCanonical));
        assert_eq!(state(0xFF80_0001), Err(WireError::NotCanonical));
        for bits in [0x7FC0_0000, 0xFFC0_0001, 0x7F80_0000, 0xFF80_0000, 0x0000_0001, 0x8000_0000, 0x3DCC_CCCD] {
            assert!(matches!(state(bits), Ok(Some(_))), "{bits:#x}");
        }
    }

    /// The failure texts carry the message with every member read so far, floats and strings in printed form.
    #[test]
    fn assertion_text_reprs_floats_strings_and_arrays() {
        let mut body = words(&[0, 1]);
        body.extend_from_slice(&0.1f32.to_le_bytes());
        let mut text = [0u8; 260];
        text[..3].copy_from_slice(b"Sim");
        body.extend_from_slice(&text);
        body.extend_from_slice(&[0; 2]);
        assert_eq!(
            decode_server(&server_frame(15, &body)).unwrap_err().to_string(),
            "2 bytes remain for <SSystemState: requestID: 0, dataInteger: 1, dataFloat: 0.10000000149011612, \
             dataString: 'Sim'>, expected a Remaining parameter"
        );
        let mut open = vec![0u8; 256];
        open[..3].copy_from_slice(b"FSX");
        open.extend_from_slice(&words(&[10, 0, 62615, 0, 10, 0, 62615, 0, 0, 0, 9]));
        assert_eq!(
            decode_server(&server_frame(2, &open)).unwrap_err().to_string(),
            "4 bytes remain for <SOpen: appName: 'FSX', appVer: [10, 0], appBuild: [62615, 0], scVer: [10, 0], \
             scBuild: [62615, 0], _ignore: 0, _ignore2: 0>, expected a Remaining parameter"
        );
    }

    /// The system-state message (id 15: request id, integer, float, a 260-byte string)
    /// and the open message (id 2: a 256-byte application name ...); strings end at the first NUL.
    #[test]
    fn system_state_and_open_decoding() {
        let mut body = words(&[0, 1]);
        body.extend_from_slice(&0.5f32.to_le_bytes());
        let mut text = [0u8; 260];
        text[..3].copy_from_slice(b"Sim");
        body.extend_from_slice(&text);
        let (state, _) = decode_server(&server_frame(15, &body)).unwrap().unwrap();
        assert_eq!(state, ServerMessage::SystemState { request_id: 0, integer: 1, string: "Sim".into() });
        let mut open = vec![0u8; 256];
        open[..4].copy_from_slice(b"KittyHawk"[..4].as_ref());
        open.extend_from_slice(&[0; 40]);
        let (message, _) = decode_server(&server_frame(2, &open)).unwrap().unwrap();
        assert_eq!(message, ServerMessage::Open { app_name: "Kitt".into() });
    }
}

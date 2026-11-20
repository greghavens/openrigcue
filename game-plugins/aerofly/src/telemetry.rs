//! Variable selection and datagram text of Aerofly_FS_2_External_DLL_Update:
//! everything between parsing the received message list and
//! the `sendto` call.

use crate::hash::*;

use crate::message::{read_message, read_string, ExternalMessage, MessageString};

/// Destination of the datagram: getaddrinfo("127.0.0.1", "4123") with
/// hints AF_INET / SOCK_DGRAM.
pub const DEST_HOST: &[u8] = b"127.0.0.1\0";
pub const DEST_PORT: &[u8] = b"4123\0";
pub const DEST_PORT_NUM: u16 = 4123;

/// Winsock arguments of the per-frame send sequence.
pub const AF_INET: i32 = 2;
pub const SOCK_DGRAM: i32 = 2;
/// Protocol argument of socket() and ai_protocol of the hints.
pub const PROTOCOL: i32 = 0;
/// bind() namelen: sizeof(sockaddr_in).
pub const BIND_ADDR_LEN: i32 = 0x10;
/// sendto() tolen: the 0x80-byte destination buffer (sockaddr_storage size).
pub const DEST_ADDR_LEN: i32 = 0x80;
/// sendto() flags.
pub const SEND_FLAGS: i32 = 0;

/// bind() address: sockaddr_in { AF_INET, port 0, INADDR_ANY, zero[8] }.
pub const fn bind_sockaddr() -> [u8; 16] {
    let mut a = [0u8; 16];
    a[0] = AF_INET as u8;
    a
}

/// The bank wrap limit, the scale of the angles and the bank wrap offset.
pub const BANK_WRAP_LIMIT: f64 = 3.0;
pub const SCALE: f64 = 1000.0;
pub const BANK_WRAP_ADD: f64 = -6.0;

/// Size of the text buffer (0x100).
pub const TEXT_BUFFER_SIZE: usize = 0x100;

/// Latest values seen in this call's message list. Every field starts at
/// 0.0 on each call.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Selected {
    pub pitch: f64,
    pub bank: f64,
    pub rate_of_turn: f64,
    pub indicated_airspeed: f64,
    pub gear: f64,
    pub angular_velocity: [f64; 3],
}

/// For each message in order, compare its hash
/// against the selected names; the last message per
/// name wins. Aircraft.Name is converted into the text
/// buffer that the formatting later clears, so its value never reaches the
/// datagram; the converted string is returned only for completeness.
pub fn select(messages: &[ExternalMessage]) -> (Selected, Option<MessageString>) {
    let mut s = Selected::default();
    let mut name = None;
    for m in messages {
        let h = m.hash();
        let v = m.value_f64(0);
        if h == AIRCRAFT_PITCH {
            s.pitch = v;
        } else if h == AIRCRAFT_BANK {
            s.bank = v;
            // above 3.0 the bank wraps (NaN -> no adjustment)
            if s.bank > BANK_WRAP_LIMIT {
                s.bank += BANK_WRAP_ADD;
            }
        } else if h == AIRCRAFT_RATE_OF_TURN {
            s.rate_of_turn = v;
        } else if h == AIRCRAFT_INDICATED_AIRSPEED {
            s.indicated_airspeed = v;
        } else if h == AIRCRAFT_GEAR {
            s.gear = v;
        } else if h == AIRCRAFT_NAME {
            name = Some(read_string(m));
        } else if h == AIRCRAFT_ANGULAR_VELOCITY {
            s.angular_velocity = [v, m.value_f64(1), m.value_f64(2)];
        }
    }
    (s, name)
}

/// `cvttsd2si r64, xmm`: truncation toward zero; NaN and out-of-range
/// inputs give the "integer indefinite" value 0x8000000000000000.
pub fn cvttsd2si(x: f64) -> i64 {
    // valid range after truncation: [-2^63, 2^63)
    if x.is_nan() || x >= 9_223_372_036_854_775_808.0 || x < -9_223_372_036_854_775_808.0 {
        i64::MIN
    } else {
        x as i64
    }
}

/// The eight integers in datagram order.
pub fn fields(s: &Selected) -> [i64; 8] {
    [
        cvttsd2si(s.pitch * SCALE),
        cvttsd2si(s.bank * SCALE),
        cvttsd2si(s.rate_of_turn * SCALE),
        cvttsd2si(s.angular_velocity[0] * SCALE),
        cvttsd2si(s.angular_velocity[1] * SCALE),
        cvttsd2si(s.angular_velocity[2] * SCALE),
        cvttsd2si(s.indicated_airspeed),
        cvttsd2si(s.gear),
    ]
}

/// The text buffer of the datagram: 0x100 bytes, NUL
/// terminated; the datagram is its strlen() bytes.
#[derive(Clone, Copy)]
pub struct TextBuffer {
    buf: [u8; TEXT_BUFFER_SIZE],
    len: usize,
}

impl TextBuffer {
    pub const fn new() -> Self {
        TextBuffer { buf: [0u8; TEXT_BUFFER_SIZE], len: 0 }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for TextBuffer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(core::str::from_utf8(self.as_bytes()).unwrap_or("<non-utf8>"))
    }
}

impl PartialEq<[u8]> for TextBuffer {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_bytes() == other
    }
}

impl core::fmt::Write for TextBuffer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        // keep room for the terminating NUL like sprintf_s
        if self.len + b.len() >= TEXT_BUFFER_SIZE {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
        self.buf[self.len] = 0;
        Ok(())
    }
}

/// "%lld;%lld;%lld;%lld;%lld;%lld;%lld;%lld". Eight i64
/// values cannot exceed 8*20+7 = 167 bytes, below the 0x100 buffer, so
/// the text is never truncated.
pub fn format_datagram(f: &[i64; 8]) -> TextBuffer {
    use core::fmt::Write;
    let mut out = TextBuffer::new();
    let _ = write!(
        out,
        "{};{};{};{};{};{};{};{}",
        f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7]
    );
    out
}

/// The message parsing loop: `num_messages` records are read
/// back-to-back starting at stream offset 0 (cursor is a u32, the stream
/// size argument is not consulted) and appended to `list` after it is
/// cleared.
pub fn collect<F: FnMut(u32) -> u8, L: MessageList + ?Sized>(mut read: F, num_messages: u32, list: &mut L) {
    list.clear();
    let mut cursor = 0u32;
    for _ in 0..num_messages {
        let m = read_message(&mut read, &mut cursor);
        list.push(&m);
    }
}

/// The static message vector as `collect` uses it. On
/// Windows it is `msvc_vector::MessageVector`; host tests use a `Vec`.
pub trait MessageList {
    fn clear(&mut self);
    fn push(&mut self, m: &ExternalMessage);
}

#[cfg(not(windows))]
impl MessageList for alloc::vec::Vec<ExternalMessage> {
    fn clear(&mut self) {
        alloc::vec::Vec::clear(self)
    }
    fn push(&mut self, m: &ExternalMessage) {
        alloc::vec::Vec::push(self, *m)
    }
}

#[cfg(windows)]
impl MessageList for crate::msvc_vector::MessageVector {
    fn clear(&mut self) {
        crate::msvc_vector::MessageVector::clear(self)
    }
    fn push(&mut self, m: &ExternalMessage) {
        // a failed allocation raises the C++ exception from here
        unsafe { self.push_back(m) }
    }
}

/// What one Update call sends: `None` when the message list is empty
/// (no socket is created at all), otherwise the datagram bytes (strlen of
/// the formatted text, no terminator).
pub fn datagram_for(list: &[ExternalMessage]) -> Option<TextBuffer> {
    if list.is_empty() {
        return None;
    }
    let (s, _name) = select(list);
    Some(format_datagram(&fields(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(hash: u64, vals: &[f64]) -> ExternalMessage {
        let mut m = ExternalMessage::with_defaults();
        m.0[0x10..0x18].copy_from_slice(&hash.to_le_bytes());
        for (i, v) in vals.iter().enumerate() {
            m.0[0x40 + 8 * i..0x48 + 8 * i].copy_from_slice(&v.to_le_bytes());
        }
        m
    }

    #[test]
    fn empty_list_sends_nothing() {
        assert!(datagram_for(&[]).is_none());
    }

    #[test]
    fn unrelated_messages_send_zeros() {
        let d = datagram_for(&[msg(tm_string_hash(b"Aircraft.Altitude"), &[1234.0])]).unwrap();
        assert_eq!(d.as_bytes(), b"0;0;0;0;0;0;0;0");
    }

    #[test]
    fn full_frame() {
        let l = [
            msg(AIRCRAFT_PITCH, &[0.1234]),
            msg(AIRCRAFT_BANK, &[-0.5]),
            msg(AIRCRAFT_RATE_OF_TURN, &[0.02]),
            msg(AIRCRAFT_ANGULAR_VELOCITY, &[0.001, -0.25, 1.5]),
            msg(AIRCRAFT_INDICATED_AIRSPEED, &[61.9]),
            msg(AIRCRAFT_GEAR, &[0.99]),
        ];
        let d = datagram_for(&l).unwrap();
        assert_eq!(d.as_bytes(), b"123;-500;20;1;-250;1500;61;0");
    }

    #[test]
    fn bank_wrap_and_last_wins() {
        let l = [msg(AIRCRAFT_BANK, &[1.0]), msg(AIRCRAFT_BANK, &[3.5])];
        assert_eq!(datagram_for(&l).unwrap().as_bytes(), b"0;-2500;0;0;0;0;0;0");
        let l = [msg(AIRCRAFT_BANK, &[3.0])];
        assert_eq!(datagram_for(&l).unwrap().as_bytes(), b"0;3000;0;0;0;0;0;0");
    }

    #[test]
    fn indefinite_integer() {
        assert_eq!(cvttsd2si(f64::NAN), i64::MIN);
        assert_eq!(cvttsd2si(1e300), i64::MIN);
        assert_eq!(cvttsd2si(-1e300), i64::MIN);
        assert_eq!(cvttsd2si(-9_223_372_036_854_775_808.0), i64::MIN);
        assert_eq!(cvttsd2si(-2.9), -2);
        let l = [msg(AIRCRAFT_GEAR, &[f64::NAN]), msg(AIRCRAFT_BANK, &[f64::NAN])];
        assert_eq!(
            datagram_for(&l).unwrap().as_bytes(),
            b"0;-9223372036854775808;0;0;0;0;0;-9223372036854775808"
        );
    }

    #[test]
    fn name_is_read_but_not_sent() {
        let mut m = msg(AIRCRAFT_NAME, &[]);
        m.0[0x19] = crate::message::DATA_TYPE_STRING8;
        m.0[0x40..0x44].copy_from_slice(b"C172");
        let (_, name) = select(&[m]);
        assert_eq!(&name.unwrap().text[..5], b"C172\0");
        assert_eq!(datagram_for(&[m]).unwrap().as_bytes(), b"0;0;0;0;0;0;0;0");
    }
}

//! `tm_external_message`: copied out of the game's
//! received byte stream, with its string values read from it.
//!
//! Record: 0x80 bytes, 0x40-byte header + 0x40-byte value area.
//! Header fields the plugin uses:
//!   +0x02 u16  total encoded size (header + value)
//!   +0x10 u64  `tm_string_hash` of the variable name
//!   +0x19 u8   data type (6 = UTF-16 string, 7 = 8-bit string)
//!   +0x40 ..   value (f64[0..8] for scalars/vectors)

pub const MESSAGE_SIZE: usize = 0x80;
pub const HEADER_SIZE: u32 = 0x40;
pub const VALUE_MAX: u32 = 0x40;

pub const DATA_TYPE_STRING16: u8 = 6;
pub const DATA_TYPE_STRING8: u8 = 7;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C, align(16))]
pub struct ExternalMessage(pub [u8; MESSAGE_SIZE]);

impl ExternalMessage {
    /// Constructor defaults written before the copy:
    /// qword0 = 0x40aaaa, qword3 = 0x10000, everything else 0.
    pub fn with_defaults() -> Self {
        let mut b = [0u8; MESSAGE_SIZE];
        b[0..8].copy_from_slice(&0x0040_aaaau64.to_le_bytes());
        b[0x18..0x20].copy_from_slice(&0x1_0000u64.to_le_bytes());
        ExternalMessage(b)
    }

    pub fn size_field(&self) -> u16 {
        u16::from_le_bytes([self.0[2], self.0[3]])
    }

    pub fn hash(&self) -> u64 {
        let mut q = [0u8; 8];
        q.copy_from_slice(&self.0[0x10..0x18]);
        u64::from_le_bytes(q)
    }

    pub fn data_type(&self) -> u8 {
        self.0[0x19]
    }

    /// `i`-th double of the value area (offset 0x40 + 8 i), i < 8.
    pub fn value_f64(&self, i: usize) -> f64 {
        let o = 0x40 + 8 * i;
        let mut q = [0u8; 8];
        q.copy_from_slice(&self.0[o..o + 8]);
        f64::from_le_bytes(q)
    }
}

/// Copies one message out of the stream.
///
/// `read(i)` returns the stream byte at absolute index `i` (the caller
/// indexes `stream + (u64)(u32)cursor` with no bounds check: it does not
/// look at the stream-size argument). The cursor is a `u32` advanced by one
/// per byte: 0x40 header bytes, then `n = (u32)size_field - 0x40`, used
/// only when `n < 0x41`, otherwise 0x40 bytes.
pub fn read_message<F: FnMut(u32) -> u8>(mut read: F, cursor: &mut u32) -> ExternalMessage {
    let mut m = ExternalMessage::with_defaults();
    for i in 0..HEADER_SIZE as usize {
        m.0[i] = read(*cursor);
        *cursor = cursor.wrapping_add(1);
    }
    let n = (m.size_field() as u32).wrapping_sub(HEADER_SIZE);
    let n = if n < VALUE_MAX + 1 { n } else { VALUE_MAX };
    for i in 0..n as usize {
        m.0[HEADER_SIZE as usize + i] = read(*cursor);
        *cursor = cursor.wrapping_add(1);
    }
    m
}

/// Output of `read_string`: `{ u32 length; char text[256]; }`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MessageString {
    pub length: u32,
    pub text: [u8; 256],
}

/// The string value of a message, narrowed to 8 bits.
/// Type 6 (UTF-16): at most 32 units, units >= 0x100 become '?'.
/// Type 7 (8-bit): at most 64 bytes. Any other type: empty.
/// Stops at the first NUL unit/byte. Text is NUL-terminated.
pub fn read_string(m: &ExternalMessage) -> MessageString {
    let mut out = MessageString { length: 0, text: [0u8; 256] };
    match m.data_type() {
        DATA_TYPE_STRING16 => {
            let mut len = 0u32;
            while len < 0xff {
                // The length test comes before the load: at len 32 the
                // unit would be 0x80 bytes in, past the record.
                if len > 0x1f {
                    break;
                }
                let o = 0x40 + len as usize * 2;
                let unit = u16::from_le_bytes([m.0[o], m.0[o + 1]]);
                if unit == 0 {
                    break;
                }
                out.text[len as usize] = if unit < 0x100 { unit as u8 } else { b'?' };
                len += 1;
                out.length = len;
            }
            out.text[len as usize] = 0;
        }
        DATA_TYPE_STRING8 => {
            let mut len = 0u32;
            while len < 0xff {
                if len > 0x3f {
                    break; // as above: the byte would be at +0x80
                }
                let c = m.0[0x40 + len as usize];
                if c == 0 {
                    break;
                }
                out.text[len as usize] = c;
                len += 1;
                out.length = len;
            }
            out.text[len as usize] = 0;
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn header(size: u16, hash: u64, ty: u8) -> Vec<u8> {
        let mut h = vec![0u8; 0x40];
        h[0] = 0xaa;
        h[1] = 0xaa;
        h[2..4].copy_from_slice(&size.to_le_bytes());
        h[0x10..0x18].copy_from_slice(&hash.to_le_bytes());
        h[0x19] = ty;
        h
    }

    fn parse(stream: &[u8]) -> (ExternalMessage, u32) {
        let mut c = 0u32;
        let m = read_message(|i| stream[i as usize], &mut c);
        (m, c)
    }

    #[test]
    fn defaults_match_constructor() {
        let m = ExternalMessage::with_defaults();
        assert_eq!(&m.0[0..8], &[0xaa, 0xaa, 0x40, 0, 0, 0, 0, 0]);
        assert_eq!(&m.0[0x18..0x20], &[0, 0, 1, 0, 0, 0, 0, 0]);
        assert!(m.0[0x20..].iter().all(|&b| b == 0));
    }

    #[test]
    fn scalar_message_consumes_header_plus_value() {
        let mut s = header(0x48, 0x1122_3344_5566_7788, 1);
        s.extend_from_slice(&1.25f64.to_le_bytes());
        s.extend_from_slice(&[0xee; 16]); // next message's bytes, not consumed
        let (m, c) = parse(&s);
        assert_eq!(c, 0x48);
        assert_eq!(m.hash(), 0x1122_3344_5566_7788);
        assert_eq!(m.value_f64(0), 1.25);
        assert_eq!(m.value_f64(1), 0.0);
    }

    #[test]
    fn value_length_capped_at_0x40() {
        let mut s = header(0x200, 1, 1);
        s.extend((0..0x100).map(|i| i as u8));
        let (m, c) = parse(&s);
        assert_eq!(c, 0x80);
        assert_eq!(&m.0[0x40..0x80], &s[0x40..0x80]);
    }

    #[test]
    fn size_below_header_underflows_to_0x40() {
        // (u32)0x10 - 0x40 wraps -> >= 0x41 -> 0x40 bytes copied
        let mut s = header(0x10, 1, 1);
        s.extend([7u8; 0x40]);
        let (_, c) = parse(&s);
        assert_eq!(c, 0x80);
    }

    #[test]
    fn exact_header_only() {
        let s = header(0x40, 1, 1);
        let (m, c) = parse(&s);
        assert_eq!(c, 0x40);
        assert!(m.0[0x40..].iter().all(|&b| b == 0));
    }

    #[test]
    fn string16_narrowing_and_limit() {
        let mut m = ExternalMessage::with_defaults();
        m.0[0x19] = DATA_TYPE_STRING16;
        let units: Vec<u16> = vec![b'C' as u16, 0x00e9, 0x0141, b'7' as u16];
        for (i, u) in units.iter().enumerate() {
            m.0[0x40 + 2 * i..0x42 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        let s = read_string(&m);
        assert_eq!(s.length, 4);
        assert_eq!(&s.text[..5], &[b'C', 0xe9, b'?', b'7', 0]);

        for i in 0..32 {
            m.0[0x40 + 2 * i..0x42 + 2 * i].copy_from_slice(&(b'a' as u16).to_le_bytes());
        }
        let s = read_string(&m);
        assert_eq!(s.length, 32);
        assert_eq!(s.text[32], 0);
    }

    #[test]
    fn string8_limit_and_other_types() {
        let mut m = ExternalMessage::with_defaults();
        m.0[0x19] = DATA_TYPE_STRING8;
        m.0[0x40..0x80].copy_from_slice(&[b'x'; 0x40]);
        let s = read_string(&m);
        assert_eq!(s.length, 0x40);
        assert_eq!(s.text[0x40], 0);
        m.0[0x19] = 1;
        let s = read_string(&m);
        assert_eq!(s.length, 0);
        assert!(s.text.iter().all(|&b| b == 0));
    }
}

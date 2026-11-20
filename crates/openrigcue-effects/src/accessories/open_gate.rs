//! The SRS device identity handshake ("open gate"): the host sends a random 14-character nonce in
//! `who_are_you`, the device answers with the hex of `XTEA-ECB(key, "<nonce><sep><status>")`. A device
//! is genuine when the answer decrypts to the nonce. The status number is parsed (the answer is
//! malformed without it) but never judged: OpenRigCue accepts a device whatever its status.

use rand::Rng;
use tracing::debug;

use super::cmd_messenger::{self, Arg, CommandSpec, Message, Value};
use super::link::SerialLink;

/// The 128-bit XTEA key shared by the belt, HaptiCR and Intellibox v6 firmwares.
pub const OPEN_GATE_KEY: [u8; 16] = [226, 109, 111, 169, 110, 194, 85, 214, 0, 16, 74, 51, 255, 24, 72, 79];

/// Length of the handshake nonce.
pub const HANDSHAKE_LENGTH: usize = 14;

/// Separator between the nonce and the status in the decrypted answer.
const SEPARATOR: char = '^';

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// A fresh nonce drawn uniformly from ASCII letters and digits.
pub fn new_handshake(rng: &mut impl Rng) -> String {
    (0..HANDSHAKE_LENGTH).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

/// Why an answer was rejected; the variants carry the reason strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    #[error("Cannot Encrypt")]
    NotHex,
    #[error("Cannot Decrypt")]
    NotBlocks,
    #[error("Error converting")]
    NotText,
    #[error("Failed to consume status")]
    NoStatusField,
    #[error("This is not a valid init command")]
    WrongHandshake,
}

/// Hex decoding: pairs of hex digits, ASCII whitespace allowed between pairs.
fn from_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let pair = bytes.get(i..i + 2)?;
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        i += 2;
    }
    Some(out)
}

/// XTEA decryption of one 8-byte block, 32 cycles (64 half-rounds), big-endian words.
fn xtea_decrypt_block(key: &[u8; 16], block: &[u8]) -> [u8; 8] {
    const DELTA: u32 = 0x9E37_79B9;
    let k: [u32; 4] = std::array::from_fn(|i| u32::from_be_bytes(key[i * 4..i * 4 + 4].try_into().unwrap()));
    let mut v0 = u32::from_be_bytes(block[0..4].try_into().unwrap());
    let mut v1 = u32::from_be_bytes(block[4..8].try_into().unwrap());
    let mut sum = DELTA.wrapping_mul(32);
    for _ in 0..32 {
        v1 =
            v1.wrapping_sub(((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0) ^ sum.wrapping_add(k[((sum >> 11) & 3) as usize]));
        sum = sum.wrapping_sub(DELTA);
        v0 = v0.wrapping_sub(((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1) ^ sum.wrapping_add(k[(sum & 3) as usize]));
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&v0.to_be_bytes());
    out[4..].copy_from_slice(&v1.to_be_bytes());
    out
}

#[cfg(test)]
fn xtea_encrypt_block(key: &[u8; 16], block: &[u8]) -> [u8; 8] {
    const DELTA: u32 = 0x9E37_79B9;
    let k: [u32; 4] = std::array::from_fn(|i| u32::from_be_bytes(key[i * 4..i * 4 + 4].try_into().unwrap()));
    let mut v0 = u32::from_be_bytes(block[0..4].try_into().unwrap());
    let mut v1 = u32::from_be_bytes(block[4..8].try_into().unwrap());
    let mut sum: u32 = 0;
    for _ in 0..32 {
        v0 = v0.wrapping_add(((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1) ^ sum.wrapping_add(k[(sum & 3) as usize]));
        sum = sum.wrapping_add(DELTA);
        v1 =
            v1.wrapping_add(((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0) ^ sum.wrapping_add(k[((sum >> 11) & 3) as usize]));
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&v0.to_be_bytes());
    out[4..].copy_from_slice(&v1.to_be_bytes());
    out
}

/// Encrypt a plaintext padded with NULs to whole blocks (what the device firmwares do), as hex. Test helper
/// for building device answers.
#[cfg(test)]
pub(crate) fn seal_for_tests(plaintext: &str) -> String {
    let mut data = plaintext.as_bytes().to_vec();
    while !data.len().is_multiple_of(8) {
        data.push(0);
    }
    data.chunks(8).flat_map(|b| xtea_encrypt_block(&OPEN_GATE_KEY, b)).map(|b| format!("{b:02x}")).collect()
}

/// Check a device's answer and return its status text.
///
/// Hex decode, XTEA-ECB decrypt, UTF-8 decode, split on `'^'`; the first
/// part must equal the nonce, the second part is the status.
pub fn open_gate(answer: &str, handshake: &str) -> Result<String, GateError> {
    let cipher = from_hex(answer).ok_or(GateError::NotHex)?;
    if cipher.len() % 8 != 0 {
        return Err(GateError::NotBlocks);
    }
    let plain: Vec<u8> = cipher.chunks(8).flat_map(|b| xtea_decrypt_block(&OPEN_GATE_KEY, b)).collect();
    let text = String::from_utf8(plain).map_err(|_| GateError::NotText)?;
    let mut parts = text.split(SEPARATOR);
    let nonce = parts.next().unwrap_or_default();
    let status = parts.next().ok_or(GateError::NoStatusField)?;
    if nonce == handshake {
        Ok(status.to_string())
    } else {
        Err(GateError::WrongHandshake)
    }
}

/// Receive attempts for the `i_am` answer (belt and HaptiCR connect loops).
const IDENTIFY_ATTEMPTS: usize = 10;

/// The verified `i_am` answer of an ESP32 accessory (`sbbbbs`: identifier, product, model, version,
/// status, serial number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrsIdentity {
    /// Status number decoded from the XTEA answer (informational only); `None` for an integer beyond 64 bits,
    /// which is accepted as well.
    pub status: Option<i64>,
    pub product: i64,
    pub model: i64,
    pub version: i64,
    pub serial: String,
}

/// The `who_are_you` receive loop: up to 10 other answers (a timeout's `None` counts) until `i_am`.
///
/// A receive error is logged ("error receiving ...") and the loop continues at once, so it does not
/// count as one of the ten attempts. Neither does a forwarded drive frame (`received_std_message`), which
/// is logged and waited past.
fn await_i_am(link: &mut dyn SerialLink, table: &[CommandSpec], i_am: usize) -> Option<Message> {
    let std_message = table.iter().position(|command| command.name == "received_std_message");
    let mut attempts = 0;
    while attempts < IDENTIFY_ATTEMPTS {
        debug!("Waiting to receive response");
        match cmd_messenger::receive(link, table) {
            Ok(Some(msg)) if msg.is(i_am) => return Some(msg),
            Ok(Some(msg)) if std_message.is_some() && msg.command == std_message => {
                debug!("receiving std message instead of device_info");
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                debug!("error receiving {e}");
                continue;
            }
        }
        attempts += 1;
    }
    debug!("ESP 32 did not respond who its");
    None
}

/// Send `who_are_you(<nonce>)`, wait for `i_am` (up to 10 reads) and verify it.
///
/// The identifier must decrypt to the
/// nonce (separator `'^'`) and the status must be an integer; `None` otherwise.
pub fn ask_identity(
    link: &mut dyn SerialLink,
    table: &[CommandSpec],
    who_are_you: usize,
    i_am: usize,
    rng: &mut impl Rng,
) -> Option<SrsIdentity> {
    let handshake = new_handshake(rng);
    if let Err(e) = cmd_messenger::send(link, table, who_are_you, &[Arg::Text(handshake.clone())]) {
        debug!("error asking who are you {e}");
        return None;
    }
    let answer = await_i_am(link, table, i_am)?;
    // `i_am` arrives with either no arguments ("error getting device name": the port is skipped)
    // or exactly the six of its format `s b b b b s`.
    let [Value::Text(identifier), Value::Int(product), Value::Int(model), Value::Int(version), _, Value::Text(serial)] =
        answer.args.as_slice()
    else {
        debug!("error getting device name");
        return None;
    };
    let status = match open_gate(identifier, &handshake) {
        // Any text that reads as an integer is accepted as the status.
        Ok(status) => match super::numeric::python_int_checked(&status) {
            Ok(status) => Some(status),
            Err(super::numeric::PyIntError::OutOfRange) => None,
            Err(super::numeric::PyIntError::Invalid) => return None,
        },
        Err(e) => {
            debug!("error decoding {e}");
            return None;
        }
    };
    Some(SrsIdentity { status, product: *product, model: *model, version: *version, serial: serial.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    /// The published XTEA test vector (key 000102..0f, plaintext 4142434445464748 -> 497df3d072612cb5).
    #[test]
    fn xtea_matches_reference_vector() {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let plain = *b"ABCDEFGH";
        let cipher = xtea_encrypt_block(&key, &plain);
        assert_eq!(cipher, [0x49, 0x7d, 0xf3, 0xd0, 0x72, 0x61, 0x2c, 0xb5]);
        assert_eq!(xtea_decrypt_block(&key, &cipher), plain);
    }

    /// A valid answer decodes to its status.
    #[test]
    fn open_gate_accepts_matching_nonce_and_returns_status() {
        let answer = seal_for_tests("aB3dE5gH7jK9mN^1");
        assert_eq!(open_gate(&answer, "aB3dE5gH7jK9mN").unwrap(), "1");
        assert_eq!(open_gate(&answer, "xxxxxxxxxxxxxx"), Err(GateError::WrongHandshake));
        let no_status = seal_for_tests("aB3dE5gH7jK9mN");
        assert_eq!(open_gate(&no_status, "aB3dE5gH7jK9mN"), Err(GateError::NoStatusField));
    }

    /// The rejection reasons, in checking order.
    #[test]
    fn open_gate_error_paths() {
        assert_eq!(open_gate("zz", "x"), Err(GateError::NotHex));
        assert_eq!(open_gate("abc", "x"), Err(GateError::NotHex));
        assert_eq!(open_gate("0011", "x"), Err(GateError::NotBlocks));
        assert_eq!(GateError::NotHex.to_string(), "Cannot Encrypt");
        assert!(from_hex("00 11\t22").is_some());
    }

    /// A nonce is 14 ASCII letters and digits.
    #[test]
    fn handshake_is_fourteen_alphanumerics() {
        let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
        let h = new_handshake(&mut rng);
        assert_eq!(h.len(), 14);
        assert!(h.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}

//! The byte layout of the rig-pose file. Everything is little-endian; the file is [`FILE_SIZE`] bytes.
//!
//! | offset | size | field | written by |
//! |-------:|-----:|-------|------------|
//! | 0   | 8  | magic `b"ORCRIGPS"` | writer, once |
//! | 8   | 4  | layout version ([`VERSION`]) | writer, once |
//! | 12  | 4  | file size ([`FILE_SIZE`]) | writer, once |
//! | 16  | 8  | seqlock sequence (odd while a write is in progress) | writer |
//! | 24  | 8  | timestamp, `CLOCK_MONOTONIC` ns | writer (payload) |
//! | 32  | 48 | six `f64` values: sway, surge, heave (mm), yaw, roll, pitch (°) | writer (payload) |
//! | 80  | 48 | six `f64` rates (per second) | writer (payload) |
//! | 128 | 24 | three `f64` centre-of-rotation offset: forward, down, right (m) | writer (payload) |
//! | 152 | 4  | flags ([`FLAG_ENABLED`], [`FLAG_RATES_VALID`]) | writer (payload) |
//! | 156 | 4  | re-centre generation | writer (payload) |
//! | 160 | 4  | writer pid | writer (payload) |
//! | 164 | 12 | reserved, zero | writer (payload) |
//! | 176 | 16 | reserved, zero | — |
//! | 192 | 8  | active-consumer claim: kind (low 32 bits), pid (high 32 bits) | consumers |
//! | 200 | 8  | active-consumer heartbeat, `CLOCK_MONOTONIC` ns | consumers |
//! | 208 | 48 | reserved, zero | — |
//!
//! The payload (bytes 24..176) is covered by the seqlock; the consumer region is written only by
//! consumers and never by the writer.

use crate::sample::{CorOffset, RigSample, RigValues};

pub const MAGIC: [u8; 8] = *b"ORCRIGPS";
pub const VERSION: u32 = 1;
pub const FILE_SIZE: u32 = 256;

pub const HEADER_LEN: usize = 16;
pub const SEQ_OFFSET: u64 = 16;
pub const PAYLOAD_OFFSET: u64 = 24;
pub const PAYLOAD_LEN: usize = 152;
pub const CONSUMER_OFFSET: u64 = 192;
pub const CONSUMER_LEN: usize = 16;

pub const FLAG_ENABLED: u32 = 1 << 0;
pub const FLAG_RATES_VALID: u32 = 1 << 1;

/// The fixed header at offset 0.
pub fn encode_header() -> [u8; HEADER_LEN] {
    let mut b = [0u8; HEADER_LEN];
    b[0..8].copy_from_slice(&MAGIC);
    b[8..12].copy_from_slice(&VERSION.to_le_bytes());
    b[12..16].copy_from_slice(&FILE_SIZE.to_le_bytes());
    b
}

/// What a header says about the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderCheck {
    Valid,
    BadMagic,
    UnsupportedVersion(u32),
    BadSize(u32),
}

pub fn check_header(b: &[u8; HEADER_LEN]) -> HeaderCheck {
    if b[0..8] != MAGIC {
        return HeaderCheck::BadMagic;
    }
    let version = u32_at(b, 8);
    if version != VERSION {
        return HeaderCheck::UnsupportedVersion(version);
    }
    let size = u32_at(b, 12);
    if size != FILE_SIZE {
        return HeaderCheck::BadSize(size);
    }
    HeaderCheck::Valid
}

/// The payload bytes (file offset [`PAYLOAD_OFFSET`]) for `sample` written by process `pid`.
pub fn encode_payload(sample: &RigSample, pid: u32) -> [u8; PAYLOAD_LEN] {
    let mut b = [0u8; PAYLOAD_LEN];
    b[0..8].copy_from_slice(&sample.timestamp_ns.to_le_bytes());
    put_f64s(&mut b[8..56], &sample.values.as_array());
    put_f64s(&mut b[56..104], &sample.rates.unwrap_or_default().as_array());
    let o = sample.cor_offset;
    put_f64s(&mut b[104..128], &[o.forward, o.down, o.right]);
    let mut flags = 0;
    if sample.enabled {
        flags |= FLAG_ENABLED;
    }
    if sample.rates.is_some() {
        flags |= FLAG_RATES_VALID;
    }
    b[128..132].copy_from_slice(&flags.to_le_bytes());
    b[132..136].copy_from_slice(&sample.recenter_generation.to_le_bytes());
    b[136..140].copy_from_slice(&pid.to_le_bytes());
    b
}

/// Decodes a payload into the sample and the writer's pid.
pub fn decode_payload(b: &[u8; PAYLOAD_LEN]) -> (RigSample, u32) {
    let flags = u32_at(b, 128);
    let rates = RigValues::from_array(f64s(&b[56..104]));
    let o: [f64; 3] = f64s(&b[104..128]);
    let sample = RigSample {
        timestamp_ns: u64_at(b, 0),
        values: RigValues::from_array(f64s(&b[8..56])),
        rates: (flags & FLAG_RATES_VALID != 0).then_some(rates),
        cor_offset: CorOffset { forward: o[0], down: o[1], right: o[2] },
        enabled: flags & FLAG_ENABLED != 0,
        recenter_generation: u32_at(b, 132),
    };
    (sample, u32_at(b, 136))
}

/// Encodes the consumer claim region.
pub fn encode_consumer(kind: u32, pid: u32, heartbeat_ns: u64) -> [u8; CONSUMER_LEN] {
    let mut b = [0u8; CONSUMER_LEN];
    let word = u64::from(kind) | (u64::from(pid) << 32);
    b[0..8].copy_from_slice(&word.to_le_bytes());
    b[8..16].copy_from_slice(&heartbeat_ns.to_le_bytes());
    b
}

/// Decodes the consumer claim region into `(kind, pid, heartbeat_ns)`.
pub fn decode_consumer(b: &[u8; CONSUMER_LEN]) -> (u32, u32, u64) {
    let word = u64_at(b, 0);
    (word as u32, (word >> 32) as u32, u64_at(b, 8))
}

fn put_f64s(dst: &mut [u8], values: &[f64]) {
    for (chunk, v) in dst.as_chunks_mut::<8>().0.iter_mut().zip(values) {
        chunk.copy_from_slice(&v.to_le_bytes());
    }
}

fn f64s<const N: usize>(src: &[u8]) -> [f64; N] {
    std::array::from_fn(|i| f64::from_le_bytes(src[i * 8..i * 8 + 8].try_into().expect("8-byte chunk")))
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4-byte field"))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8-byte field"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_consistent() {
        assert_eq!(PAYLOAD_OFFSET as usize + PAYLOAD_LEN, 176);
        assert!(176 <= CONSUMER_OFFSET as usize);
        assert!(CONSUMER_OFFSET as usize + CONSUMER_LEN <= FILE_SIZE as usize);
    }

    #[test]
    fn payload_round_trips_and_has_the_documented_offsets() {
        let s = RigSample {
            timestamp_ns: 0x0102_0304_0506_0708,
            values: RigValues { sway: 1.0, surge: 2.0, heave: 3.0, yaw: 4.0, roll: 5.0, pitch: 6.0 },
            rates: Some(RigValues { sway: -1.0, surge: -2.0, heave: -3.0, yaw: -4.0, roll: -5.0, pitch: -6.0 }),
            cor_offset: CorOffset { forward: 0.1, down: 0.6, right: -0.05 },
            enabled: true,
            recenter_generation: 7,
        };
        let b = encode_payload(&s, 4242);
        assert_eq!(decode_payload(&b), (s, 4242));
        // File offset 32 = payload offset 8: sway, the first value.
        assert_eq!(f64::from_le_bytes(b[8..16].try_into().unwrap()), 1.0);
        assert_eq!(f64::from_le_bytes(b[48..56].try_into().unwrap()), 6.0);
        assert_eq!(u32_at(&b, 128), FLAG_ENABLED | FLAG_RATES_VALID);
        let no_rates = RigSample { rates: None, enabled: false, ..s };
        let b = encode_payload(&no_rates, 1);
        assert_eq!(u32_at(&b, 128), 0);
        assert_eq!(decode_payload(&b).0, no_rates);
    }

    #[test]
    fn header_checks() {
        let mut h = encode_header();
        assert_eq!(check_header(&h), HeaderCheck::Valid);
        h[8] = 9;
        assert_eq!(check_header(&h), HeaderCheck::UnsupportedVersion(9));
        h[0] = b'x';
        assert_eq!(check_header(&h), HeaderCheck::BadMagic);
    }

    #[test]
    fn consumer_round_trips() {
        let b = encode_consumer(2, 0xdead_beef, 99);
        assert_eq!(decode_consumer(&b), (2, 0xdead_beef, 99));
    }
}

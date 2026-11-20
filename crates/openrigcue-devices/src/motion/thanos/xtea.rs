//! XTEA block cipher as used by the Thanos authentication handshake.
//!
//! 64 rounds (32 cycles), big-endian words, ECB.

const DELTA: u32 = 0x9E37_79B9;
const CYCLES: u32 = 32;

fn key_words(key: &[u8; 16]) -> [u32; 4] {
    let mut k = [0u32; 4];
    for (i, word) in k.iter_mut().enumerate() {
        *word = u32::from_be_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    k
}

fn split_block(block: &[u8]) -> (u32, u32) {
    (
        u32::from_be_bytes([block[0], block[1], block[2], block[3]]),
        u32::from_be_bytes([block[4], block[5], block[6], block[7]]),
    )
}

/// Encrypts one 8-byte block.
fn encrypt_block(k: &[u32; 4], block: &[u8]) -> [u8; 8] {
    let (mut v0, mut v1) = split_block(block);
    let mut sum = 0u32;
    for _ in 0..CYCLES {
        v0 = v0.wrapping_add(((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1) ^ sum.wrapping_add(k[(sum & 3) as usize]));
        sum = sum.wrapping_add(DELTA);
        v1 =
            v1.wrapping_add(((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0) ^ sum.wrapping_add(k[((sum >> 11) & 3) as usize]));
    }
    join_block(v0, v1)
}

/// Decrypts one 8-byte block.
fn decrypt_block(k: &[u32; 4], block: &[u8]) -> [u8; 8] {
    let (mut v0, mut v1) = split_block(block);
    let mut sum = DELTA.wrapping_mul(CYCLES);
    for _ in 0..CYCLES {
        v1 =
            v1.wrapping_sub(((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0) ^ sum.wrapping_add(k[((sum >> 11) & 3) as usize]));
        sum = sum.wrapping_sub(DELTA);
        v0 = v0.wrapping_sub(((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1) ^ sum.wrapping_add(k[(sum & 3) as usize]));
    }
    join_block(v0, v1)
}

fn join_block(v0: u32, v1: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&v0.to_be_bytes());
    out[4..].copy_from_slice(&v1.to_be_bytes());
    out
}

/// ECB encryption. `None` when the data is not a whole number of 8-byte blocks.
pub fn encrypt_ecb(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    let k = key_words(key);
    data.len().is_multiple_of(8).then(|| data.chunks(8).flat_map(|b| encrypt_block(&k, b)).collect())
}

/// ECB decryption. `None` when the data is not a whole number of 8-byte blocks.
pub fn decrypt_ecb(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    let k = key_words(key);
    data.len().is_multiple_of(8).then(|| data.chunks(8).flat_map(|b| decrypt_block(&k, b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published XTEA test vector (32 cycles, big-endian): key 000102..0f, plaintext 4142434445464748.
    #[test]
    fn xtea_encrypt_matches_reference_vector() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let c = encrypt_ecb(&key, b"ABCDEFGH").unwrap();
        assert_eq!(c, [0x49, 0x7d, 0xf3, 0xd0, 0x72, 0x61, 0x2c, 0xb5]);
        assert_eq!(decrypt_ecb(&key, &c).unwrap(), b"ABCDEFGH");
    }

    /// Second published vector: all-zero key, plaintext 4141414141414141.
    #[test]
    fn xtea_zero_key_vector() {
        let key = [0u8; 16];
        let c = encrypt_ecb(&key, b"AAAAAAAA").unwrap();
        assert_eq!(c, [0xed, 0x23, 0x37, 0x5a, 0x82, 0x1a, 0x8c, 0x2d]);
    }

    /// ECB rejects partial blocks; the handshake treats that as an error.
    #[test]
    fn xtea_rejects_partial_block() {
        assert!(encrypt_ecb(&[0; 16], b"short").is_none());
        assert!(decrypt_ecb(&[0; 16], &[0; 9]).is_none());
    }
}

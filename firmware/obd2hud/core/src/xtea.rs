//! The XTEA block cipher in ECB mode.
//! Key words and data halves are big-endian; one "round" here is a
//! full XTEA cycle (two Feistel half-rounds), the firmware uses 32.

const DELTA: u32 = 0x9e37_79b9;

/// The key schedule: four big-endian 32-bit words.
pub fn key_schedule(key: &[u8; 16]) -> [u32; 4] {
    let mut k = [0u32; 4];
    for (i, w) in k.iter_mut().enumerate() {
        *w = u32::from_be_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    k
}

pub fn encrypt_block(k: &[u32; 4], block: &mut [u8; 8], rounds: u32) {
    let mut v0 = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
    let mut v1 = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);
    let mut sum: u32 = 0;
    for _ in 0..rounds {
        v0 = v0.wrapping_add((((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1)) ^ sum.wrapping_add(k[(sum & 3) as usize]));
        sum = sum.wrapping_add(DELTA);
        v1 = v1
            .wrapping_add((((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0)) ^ sum.wrapping_add(k[((sum >> 11) & 3) as usize]));
    }
    block[..4].copy_from_slice(&v0.to_be_bytes());
    block[4..].copy_from_slice(&v1.to_be_bytes());
}

pub fn decrypt_block(k: &[u32; 4], block: &mut [u8; 8], rounds: u32) {
    let mut v0 = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
    let mut v1 = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);
    let mut sum: u32 = DELTA.wrapping_mul(rounds);
    for _ in 0..rounds {
        v1 = v1
            .wrapping_sub((((v0 << 4) ^ (v0 >> 5)).wrapping_add(v0)) ^ sum.wrapping_add(k[((sum >> 11) & 3) as usize]));
        sum = sum.wrapping_sub(DELTA);
        v0 = v0.wrapping_sub((((v1 << 4) ^ (v1 >> 5)).wrapping_add(v1)) ^ sum.wrapping_add(k[(sum & 3) as usize]));
    }
    block[..4].copy_from_slice(&v0.to_be_bytes());
    block[4..].copy_from_slice(&v1.to_be_bytes());
}

/// ECB encryption in place. Returns false (and leaves the data
/// untouched) when the length is not a multiple of 8.
pub fn ecb_encrypt(key: &[u8; 16], data: &mut [u8], rounds: u32) -> bool {
    if data.len() % 8 != 0 {
        return false;
    }
    let k = key_schedule(key);
    for chunk in data.chunks_exact_mut(8) {
        let mut b = [0u8; 8];
        b.copy_from_slice(chunk);
        encrypt_block(&k, &mut b, rounds);
        chunk.copy_from_slice(&b);
    }
    true
}

pub fn ecb_decrypt(key: &[u8; 16], data: &mut [u8], rounds: u32) -> bool {
    if data.len() % 8 != 0 {
        return false;
    }
    let k = key_schedule(key);
    for chunk in data.chunks_exact_mut(8) {
        let mut b = [0u8; 8];
        b.copy_from_slice(chunk);
        decrypt_block(&k, &mut b, rounds);
        chunk.copy_from_slice(&b);
    }
    true
}

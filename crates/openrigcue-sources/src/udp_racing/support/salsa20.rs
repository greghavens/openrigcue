//! Salsa20/20 keystream XOR:
//! 32-byte key, 8-byte nonce, block counter starting at 0, little-endian words.

fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[b] ^= s[a].wrapping_add(s[d]).rotate_left(7);
    s[c] ^= s[b].wrapping_add(s[a]).rotate_left(9);
    s[d] ^= s[c].wrapping_add(s[b]).rotate_left(13);
    s[a] ^= s[d].wrapping_add(s[c]).rotate_left(18);
}

fn words(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
}

fn block(key: &[u8; 32], nonce: &[u8; 8], counter: u64) -> [u8; 64] {
    let c: Vec<u32> = words(b"expand 32-byte k").collect();
    let k: Vec<u32> = words(key).collect();
    let n: Vec<u32> = words(nonce).collect();
    let input = [
        c[0],
        k[0],
        k[1],
        k[2],
        k[3],
        c[1],
        n[0],
        n[1],
        counter as u32,
        (counter >> 32) as u32,
        c[2],
        k[4],
        k[5],
        k[6],
        k[7],
        c[3],
    ];
    let mut s = input;
    for _ in 0..10 {
        quarter_round(&mut s, 0, 4, 8, 12);
        quarter_round(&mut s, 5, 9, 13, 1);
        quarter_round(&mut s, 10, 14, 2, 6);
        quarter_round(&mut s, 15, 3, 7, 11);
        quarter_round(&mut s, 0, 1, 2, 3);
        quarter_round(&mut s, 5, 6, 7, 4);
        quarter_round(&mut s, 10, 11, 8, 9);
        quarter_round(&mut s, 15, 12, 13, 14);
    }
    let mut out = [0u8; 64];
    for (i, (w, orig)) in s.iter().zip(input).enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&w.wrapping_add(orig).to_le_bytes());
    }
    out
}

/// XOR `message` with the Salsa20 keystream.
pub fn xor(key: &[u8; 32], nonce: &[u8; 8], message: &[u8]) -> Vec<u8> {
    message
        .chunks(64)
        .enumerate()
        .flat_map(|(i, chunk)| {
            let stream = block(key, nonce, i as u64);
            chunk.iter().zip(stream).map(|(m, s)| m ^ s).collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_ecrypt_reference_vector() {
        // ECRYPT Salsa20/20 256-bit test vector set 1, vector 0: key 0x80 00.., IV 0, first keystream bytes.
        let mut key = [0u8; 32];
        key[0] = 0x80;
        let stream = xor(&key, &[0u8; 8], &[0u8; 8]);
        assert_eq!(stream, [0xE3, 0xBE, 0x8F, 0xDD, 0x8B, 0xEC, 0xA2, 0xE3]);
    }

    #[test]
    fn xor_is_its_own_inverse_across_blocks() {
        let key = *b"Simulator Interface Packet GT7 v";
        let nonce = [1, 2, 3, 4, 5, 6, 7, 8];
        let msg: Vec<u8> = (0..200u8).collect();
        assert_eq!(xor(&key, &nonce, &xor(&key, &nonce, &msg)), msg);
    }
}

//! The handshake answer:
//! the `i_am` identifier = lower-case hex of XTEA-ECB(key, 16-byte plaintext),
//! plaintext = first 14 handshake bytes + "^<status>".

use crate::consts::{XTEA_KEY, XTEA_ROUNDS};
use crate::random::random_range;
use crate::xtea;

/// Builds the 16-byte plaintext. `handshake` is the C string the PC sent
/// (not unescaped). Bytes past the end of the text are not written: on the
/// device they are whatever the stack holds;
/// host fixtures can provide them with plaintext_with_tail.
pub fn plaintext<R: FnMut() -> u32>(handshake: &[u8], license_status: u8, esp_random: &mut R) -> [u8; 16] {
    #[cfg(target_arch = "xtensa")]
    {
        // The plaintext is not initialized beyond its written
        // prefix. Its remaining machine bytes are read
        // through register outputs, without reading uninitialized Rust.
        let mut storage = core::mem::MaybeUninit::<[u8; 16]>::uninit();
        let pointer = storage.as_mut_ptr().cast::<u8>();
        write_plaintext(handshake, license_status, esp_random, |i, value| unsafe {
            pointer.add(i).write_volatile(value)
        });
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            let value: u32;
            unsafe {
                core::arch::asm!("l8ui {value},{pointer},0",value=out(reg)value,pointer=in(reg)pointer.add(i),options(readonly,nostack));
            }
            *byte = value as u8;
        }
        return out;
    }
    #[cfg(not(target_arch = "xtensa"))]
    {
        plaintext_with_tail(handshake, license_status, esp_random, [0; 16])
    }
}

/// [`plaintext`] with the initial values of the unwritten bytes given
/// explicitly, for deterministic host tests.
pub fn plaintext_with_tail<R: FnMut() -> u32>(
    handshake: &[u8],
    license_status: u8,
    esp_random: &mut R,
    mut out: [u8; 16],
) -> [u8; 16] {
    write_plaintext(handshake, license_status, esp_random, |i, value| out[i] = value);
    out
}

fn write_plaintext<R: FnMut() -> u32>(
    handshake: &[u8],
    license_status: u8,
    esp_random: &mut R,
    mut write: impl FnMut(usize, u8),
) {
    // 16 characters plus the NUL of a 16-character result.
    let mut data = [0u8; 17];
    // At most 14 handshake bytes, then a NUL.
    let len = handshake.iter().position(|&b| b == 0).unwrap_or(handshake.len());
    let n = len.min(14);
    data[..n].copy_from_slice(&handshake[..n]);
    data[n] = 0;
    // status 0/1/2 appends "^0"/"^1"/"^2"; any other value appends nothing.
    let suffix: &[u8] = match license_status {
        0 => b"^0",
        1 => b"^1",
        2 => b"^2",
        _ => b"",
    };
    data[n..n + suffix.len()].copy_from_slice(suffix);
    data[n + suffix.len()] = 0;
    let strlen = data.iter().position(|&b| b == 0).unwrap_or(16);
    for i in 0..strlen {
        let value = if data[i] == b'?' {
            let r = random_range(0x28, 0x7e, esp_random);
            (if r == 0x5e { 0x5d } else { r }) as u8
        } else {
            data[i]
        };
        write(i, value);
    }
}

/// The full handshake answer: 32 lower-case hex characters.
pub fn open_gate<R: FnMut() -> u32>(handshake: &[u8], license_status: u8, esp_random: &mut R) -> [u8; 32] {
    let mut block = plaintext(handshake, license_status, esp_random);
    xtea::ecb_encrypt(&XTEA_KEY, &mut block, XTEA_ROUNDS);
    hex_lower(&block)
}

/// Two lower-case hex digits per byte.
pub fn hex_lower<const N: usize, const M: usize>(bytes: &[u8; N]) -> [u8; M] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; M];
    for (i, b) in bytes.iter().enumerate() {
        out[2 * i] = HEX[(b >> 4) as usize];
        out[2 * i + 1] = HEX[(b & 0xf) as usize];
    }
    out
}

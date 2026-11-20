//! The common text decoders (native byte order is little-endian): UTF-8, UTF-16,
//! UTF-32, UTF-7, ASCII, Latin-1, single-byte character maps, and the Windows code
//! pages of `mbcs`/`oem`.

use super::{Errors, NativeException};
use crate::patch::pycodecs;

fn is_continuation(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

fn is_surrogate(c: u32) -> bool {
    (0xD800..0xE000).contains(&c)
}

/// Runs the handler and resumes where it says.
fn recover(
    errors: Errors,
    encoding: &str,
    data: &[u8],
    (start, end, reason): (usize, usize, &str),
    out: &mut Vec<u32>,
) -> Result<usize, NativeException> {
    let (replacement, position) = errors.handle(encoding, data, start, end, reason)?;
    out.extend(replacement);
    Ok(position)
}

/// UTF-8 decoding of complete input, classifying each invalid sequence.
pub(super) fn utf8(data: &[u8], errors: Errors) -> Result<Vec<u32>, NativeException> {
    let n = data.len();
    let mut out = Vec::with_capacity(n);
    let mut s = 0;
    while s < n {
        let c = data[s];
        if c < 0x80 {
            out.push(u32::from(c));
            s += 1;
            continue;
        }
        let left = n - s;
        let byte = |i: usize| data[s + i];
        const END: u8 = 0;
        // 0: unexpected end, 1: invalid start, 2..=4: invalid continuation at byte 1..=3.
        let failure: u8 = if c < 0xC2 {
            1
        } else if c < 0xE0 {
            if left < 2 {
                END
            } else if !is_continuation(byte(1)) {
                2
            } else {
                out.push((u32::from(c & 0x1F) << 6) | u32::from(byte(1) & 0x3F));
                s += 2;
                continue;
            }
        } else if c < 0xF0 {
            if left < 3 {
                if left < 2 {
                    END
                } else {
                    let ch2 = byte(1);
                    if !is_continuation(ch2) || if ch2 < 0xA0 { c == 0xE0 } else { c == 0xED } {
                        2
                    } else {
                        END
                    }
                }
            } else {
                let (ch2, ch3) = (byte(1), byte(2));
                if !is_continuation(ch2) || (c == 0xE0 && ch2 < 0xA0) || (c == 0xED && ch2 >= 0xA0) {
                    2
                } else if !is_continuation(ch3) {
                    3
                } else {
                    out.push((u32::from(c & 0x0F) << 12) | (u32::from(ch2 & 0x3F) << 6) | u32::from(ch3 & 0x3F));
                    s += 3;
                    continue;
                }
            }
        } else if c < 0xF5 {
            if left < 4 {
                if left < 2 {
                    END
                } else {
                    let ch2 = byte(1);
                    if !is_continuation(ch2) || if ch2 < 0x90 { c == 0xF0 } else { c == 0xF4 } {
                        2
                    } else if left < 3 {
                        END
                    } else if !is_continuation(byte(2)) {
                        3
                    } else {
                        END
                    }
                }
            } else {
                let (ch2, ch3, ch4) = (byte(1), byte(2), byte(3));
                if !is_continuation(ch2) || (c == 0xF0 && ch2 < 0x90) || (c == 0xF4 && ch2 >= 0x90) {
                    2
                } else if !is_continuation(ch3) {
                    3
                } else if !is_continuation(ch4) {
                    4
                } else {
                    out.push(
                        (u32::from(c & 0x07) << 18)
                            | (u32::from(ch2 & 0x3F) << 12)
                            | (u32::from(ch3 & 0x3F) << 6)
                            | u32::from(ch4 & 0x3F),
                    );
                    s += 4;
                    continue;
                }
            }
        } else {
            1
        };
        let error = match failure {
            END => (s, n, "unexpected end of data"),
            1 => (s, s + 1, "invalid start byte"),
            k => (s, s + usize::from(k) - 1, "invalid continuation byte"),
        };
        s = recover(errors, "utf-8", data, error, &mut out)?;
    }
    Ok(out)
}

/// UTF-16 decoding of complete input: `bo` 0 reads a byte-order mark (default
/// little-endian), -1 little-endian, 1 big-endian.
pub(super) fn utf16(data: &[u8], errors: Errors, mut bo: i32) -> Result<Vec<u32>, NativeException> {
    let n = data.len();
    let mut q = 0;
    if bo == 0 && n >= 2 {
        match u16::from_le_bytes([data[0], data[1]]) {
            0xFEFF => {
                q = 2;
                bo = -1;
            }
            0xFFFE => {
                q = 2;
                bo = 1;
            }
            _ => {}
        }
    }
    let little = bo <= 0;
    let encoding = if little { "utf-16-le" } else { "utf-16-be" };
    let unit = |at: usize| {
        u32::from(if little {
            u16::from_le_bytes([data[at], data[at + 1]])
        } else {
            u16::from_be_bytes([data[at], data[at + 1]])
        })
    };
    let mut out = Vec::with_capacity(n / 2);
    while q < n {
        let error = if n - q < 2 {
            (q, n, "truncated data")
        } else {
            let ch = unit(q);
            q += 2;
            if !is_surrogate(ch) {
                out.push(ch);
                continue;
            }
            if ch >= 0xDC00 {
                (q - 2, q, "illegal encoding")
            } else if n - q < 2 {
                q -= 2;
                (q, n, "unexpected end of data")
            } else {
                let ch2 = unit(q);
                q += 2;
                if !(0xDC00..0xE000).contains(&ch2) {
                    (q - 4, q - 2, "illegal UTF-16 surrogate")
                } else {
                    out.push(0x10000 + ((ch - 0xD800) << 10) + (ch2 - 0xDC00));
                    continue;
                }
            }
        };
        q = recover(errors, encoding, data, error, &mut out)?;
    }
    Ok(out)
}

/// UTF-32 decoding of complete input, with the same byte-order parameter.
pub(super) fn utf32(data: &[u8], errors: Errors, mut bo: i32) -> Result<Vec<u32>, NativeException> {
    let n = data.len();
    let mut q = 0;
    if bo == 0 && n >= 4 {
        match u32::from_le_bytes([data[0], data[1], data[2], data[3]]) {
            0x0000_FEFF => {
                bo = -1;
                q = 4;
            }
            0xFFFE_0000 => {
                bo = 1;
                q = 4;
            }
            _ => {}
        }
    }
    let little = bo <= 0;
    let encoding = if little { "utf-32-le" } else { "utf-32-be" };
    let mut out = Vec::with_capacity(n / 4);
    while q < n {
        let error = if n - q < 4 {
            (q, n, "truncated data")
        } else {
            let bytes = [data[q], data[q + 1], data[q + 2], data[q + 3]];
            let ch = if little { u32::from_le_bytes(bytes) } else { u32::from_be_bytes(bytes) };
            if is_surrogate(ch) {
                (q, q + 4, "code point in surrogate code point range(0xd800, 0xe000)")
            } else if ch < 0x110000 {
                out.push(ch);
                q += 4;
                continue;
            } else {
                (q, q + 4, "code point not in range(0x110000)")
            }
        };
        q = recover(errors, encoding, data, error, &mut out)?;
    }
    Ok(out)
}

fn is_base64(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'+' || c == b'/'
}

fn from_base64(c: u8) -> u32 {
    u32::from(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    })
}

/// UTF-7 decoding of complete input.
pub(super) fn utf7(data: &[u8], errors: Errors) -> Result<Vec<u32>, NativeException> {
    let e = data.len();
    let mut out = Vec::with_capacity(e);
    let mut s = 0;
    let mut in_shift = false;
    let mut start = 0;
    let (mut bits, mut buffer, mut surrogate) = (0u32, 0u32, 0u32);
    loop {
        while s < e {
            let ch = data[s];
            let reason;
            if in_shift {
                if is_base64(ch) {
                    buffer = (buffer << 6) | from_base64(ch);
                    bits += 6;
                    s += 1;
                    if bits >= 16 {
                        let out_ch = buffer >> (bits - 16);
                        bits -= 16;
                        buffer &= (1 << bits) - 1;
                        if surrogate != 0 {
                            if (0xDC00..0xE000).contains(&out_ch) {
                                out.push(0x10000 + ((surrogate - 0xD800) << 10) + (out_ch - 0xDC00));
                                surrogate = 0;
                                continue;
                            }
                            out.push(surrogate);
                            surrogate = 0;
                        }
                        if (0xD800..0xDC00).contains(&out_ch) {
                            surrogate = out_ch;
                        } else {
                            out.push(out_ch);
                        }
                    }
                    continue;
                }
                in_shift = false;
                if bits >= 6 {
                    s += 1;
                    reason = "partial character in shift sequence";
                } else if bits > 0 && buffer != 0 {
                    s += 1;
                    reason = "non-zero padding bits in shift sequence";
                } else {
                    if surrogate != 0 && ch <= 127 && ch != b'+' {
                        out.push(surrogate);
                    }
                    surrogate = 0;
                    if ch == b'-' {
                        s += 1;
                    }
                    continue;
                }
            } else if ch == b'+' {
                start = s;
                s += 1;
                if s < e && data[s] == b'-' {
                    s += 1;
                    out.push(u32::from(b'+'));
                    continue;
                } else if s < e && !is_base64(data[s]) {
                    s += 1;
                    reason = "ill-formed sequence";
                } else {
                    in_shift = true;
                    surrogate = 0;
                    bits = 0;
                    buffer = 0;
                    continue;
                }
            } else if ch <= 127 {
                s += 1;
                out.push(u32::from(ch));
                continue;
            } else {
                start = s;
                s += 1;
                reason = "unexpected special character";
            }
            s = recover(errors, "utf7", data, (start, s, reason), &mut out)?;
        }
        if in_shift {
            in_shift = false;
            if surrogate != 0 || bits >= 6 || (bits > 0 && buffer != 0) {
                s = recover(errors, "utf7", data, (start, e, "unterminated shift sequence"), &mut out)?;
                if s < e {
                    continue;
                }
            }
        }
        return Ok(out);
    }
}

/// ASCII decoding.
pub(super) fn ascii(data: &[u8], errors: Errors) -> Result<Vec<u32>, NativeException> {
    let mut out = Vec::with_capacity(data.len());
    let mut s = 0;
    while s < data.len() {
        if data[s] < 128 {
            out.push(u32::from(data[s]));
            s += 1;
            continue;
        }
        s = recover(errors, "ascii", data, (s, s + 1, "ordinal not in range(128)"), &mut out)?;
    }
    Ok(out)
}

/// Latin-1 decoding.
pub(super) fn latin1(data: &[u8]) -> Vec<u32> {
    data.iter().map(|byte| u32::from(*byte)).collect()
}

/// Character-map decoding with the table of a single-byte codec module.
pub(super) fn charmap(data: &[u8], module: &str, errors: Errors) -> Result<Vec<u32>, NativeException> {
    let Some(table) = pycodecs::byte_table(module) else {
        // Every text codec module is handled; a module without a table cannot be named here.
        return Err(super::error("LookupError", format!("unknown encoding: {module}")));
    };
    let mut out = Vec::with_capacity(data.len());
    let mut s = 0;
    while s < data.len() {
        if let Some(c) = table[usize::from(data[s])] {
            out.push(u32::from(c));
            s += 1;
            continue;
        }
        s = recover(errors, "charmap", data, (s, s + 1, "character maps to <undefined>"), &mut out)?;
    }
    Ok(out)
}

/// `mbcs` / `oem` (code pages 1252 / 437): every byte of both code pages maps to a character.
pub(super) fn code_page(data: &[u8], module: &str) -> Result<Vec<u32>, NativeException> {
    let table = pycodecs::byte_table(module).expect("Windows code page table");
    let mut out = Vec::with_capacity(data.len());
    for (index, byte) in data.iter().enumerate() {
        match table[usize::from(*byte)] {
            Some(c) => out.push(u32::from(c)),
            None => {
                return Err(super::decode_error(
                    module,
                    data,
                    index as i64,
                    index as i64 + 1,
                    "No mapping for the Unicode character exists in the target code page.",
                ))
            }
        }
    }
    Ok(out)
}

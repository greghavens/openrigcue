//! The telemetry characteristic (`c97c6c5a-…`, WRITE) and what a write to it does.
//!
//! The written value, up to its first NUL, is split at `'~'` into nine fields and
//! each field is converted to a float slot. A missing field counts as `"0"`.
//! A field is parsed as a double and then narrowed to a float.
//!
//! The host writes the line power, speed, water temperature, RPM,
//! turbo pressure, fuel level, oil temperature, oil pressure, vacuum.

use alloc::vec::Vec;

/// Number of float slots a write fills.
pub const SLOT_COUNT: usize = 9;
/// The field separator of the telemetry line.
pub const SEPARATOR: u8 = b'~';
/// The text a missing field counts as.
pub const MISSING_FIELD: &[u8] = b"0";

/// Slot indices.
pub mod slot {
    /// HUD power; the main loop drives GPIO19 high when it is 1.0.
    pub const POWER: usize = 0;
    /// PID 0x0D.
    pub const SPEED: usize = 1;
    /// PID 0x05.
    pub const COOLANT: usize = 2;
    /// PID 0x0C.
    pub const RPM: usize = 3;
    /// PID 0x0B (the host sends the turbo pressure here).
    pub const MAP: usize = 4;
    /// PID 0x2F.
    pub const FUEL_LEVEL: usize = 5;
    /// PID 0x5C.
    pub const OIL_TEMP: usize = 6;
    /// PID 0x0A (the host sends the oil pressure here).
    pub const FUEL_PRESSURE: usize = 7;
    /// PID 0x33 (the host sends the vacuum here).
    pub const BARO: usize = 8;
}

/// The bytes up to the first NUL.
pub fn c_string(bytes: &[u8]) -> &[u8] {
    let n = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..n]
}

/// The part of `s` from `left` up to `right`; the bounds are swapped when reversed and
/// clamped to the length.
pub fn substring(s: &[u8], left: u32, right: u32) -> &[u8] {
    let (mut l, mut r) = (left, right);
    if r < l {
        core::mem::swap(&mut l, &mut r);
    }
    let len = s.len() as u32;
    if l >= len {
        return &[];
    }
    let r = r.min(len);
    &s[l as usize..r as usize]
}

/// Field `index` of `data` split at `separator`, or [`MISSING_FIELD`].
pub fn get_value(data: &[u8], separator: u8, index: i32) -> &[u8] {
    let max_index = data.len() as i32 - 1;
    let mut found = 0i32;
    let mut str_index0 = 0i32;
    let mut str_index1 = -1i32;
    let mut i = 0i32;
    while i <= max_index && found <= index {
        if data[i as usize] == separator || i == max_index {
            found += 1;
            str_index0 = str_index1 + 1;
            str_index1 = if i == max_index { i + 1 } else { i };
        }
        i += 1;
    }
    if found > index {
        substring(data, str_index0 as u32, str_index1 as u32)
    } else {
        MISSING_FIELD
    }
}

/// The field as a float: parsed as a double, then narrowed.
pub fn to_float(s: &[u8]) -> f32 {
    #[cfg(target_arch = "xtensa")]
    {
        unsafe extern "C" {
            fn atof(s: *const core::ffi::c_char) -> f64;
        }
        let s = c_string(s);
        // A short field needs no allocation. For a longer field a failed
        // allocation gives 0 rather than an abort.
        if s.len() < 12 {
            let mut text = [0u8; 12];
            text[..s.len()].copy_from_slice(s);
            return unsafe { atof(text.as_ptr().cast()) as f32 };
        }
        let mut text = Vec::new();
        let Some(size) = s.len().checked_add(1) else {
            return 0.0;
        };
        if text.try_reserve_exact(size).is_err() {
            return 0.0;
        }
        text.extend_from_slice(s);
        text.push(0);
        return unsafe { atof(text.as_ptr().cast()) as f32 };
    }
    #[cfg(not(target_arch = "xtensa"))]
    {
        strtod(s) as f32
    }
}

/// The nine slot values of one written value.
pub fn parse(value: &[u8]) -> [f32; SLOT_COUNT] {
    let mut out = [0f32; SLOT_COUNT];
    publish(value, |i, slot| out[i] = slot);
    out
}

/// Publishes each converted field before starting the next one. The BLE task and
/// CAN loop share individual float slots, not an atomic nine-field snapshot.
pub fn publish(value: &[u8], mut write: impl FnMut(usize, f32)) {
    let s = c_string(value);
    for i in 0..SLOT_COUNT {
        write(i, to_float(get_value(s, SEPARATOR, i as i32)));
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn starts_with_ci(s: &[u8], word: &[u8]) -> bool {
    s.len() >= word.len() && s[..word.len()].eq_ignore_ascii_case(word)
}

/// Parses a double the way C `strtod` does: leading white space, optional sign, then a
/// decimal or `0x` hexadecimal floating constant, `inf`/`infinity` or `nan[(…)]`; the longest
/// valid prefix is converted with round-to-nearest; no valid prefix gives 0.
pub fn strtod(s: &[u8]) -> f64 {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let rest = &s[i..];
    let v = if starts_with_ci(rest, b"inf") {
        f64::INFINITY
    } else if starts_with_ci(rest, b"nan") {
        f64::NAN
    } else if rest.len() >= 3
        && rest[0] == b'0'
        && (rest[1] | 0x20) == b'x'
        && (rest[2].is_ascii_hexdigit() || (rest[2] == b'.' && rest.get(3).is_some_and(|c| c.is_ascii_hexdigit())))
    {
        hex_float(&rest[2..])
    } else {
        decimal(rest)
    };
    if neg {
        -v
    } else {
        v
    }
}

fn decimal(s: &[u8]) -> f64 {
    let mut i = 0;
    let int_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < s.len() && s[i] == b'.' {
        let j = i + 1;
        let mut k = j;
        while k < s.len() && s[k].is_ascii_digit() {
            k += 1;
        }
        frac_digits = k - j;
        if int_digits > 0 || frac_digits > 0 {
            i = k;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return 0.0;
    }
    // Exponent only when at least one digit follows.
    if i < s.len() && (s[i] | 0x20) == b'e' {
        let mut k = i + 1;
        if k < s.len() && (s[k] == b'+' || s[k] == b'-') {
            k += 1;
        }
        let ds = k;
        while k < s.len() && s[k].is_ascii_digit() {
            k += 1;
        }
        if k > ds {
            i = k;
        }
    }
    // Rust's parser is correctly rounded. It wants a
    // digit before an exponent-less trailing '.', which "1." already has.
    let mut buf: Vec<u8> = Vec::with_capacity(i + 1);
    if int_digits == 0 {
        buf.push(b'0');
    }
    buf.extend_from_slice(&s[..i]);
    core::str::from_utf8(&buf).ok().and_then(|t| t.parse::<f64>().ok()).unwrap_or(0.0)
}

/// Hexadecimal significand (after "0x") with optional binary exponent `p[+-]d+`.
fn hex_float(s: &[u8]) -> f64 {
    let mut mant: u64 = 0;
    let mut exp: i64 = 0;
    let mut sticky = false;
    let mut i = 0;
    let mut seen_point = false;
    while i < s.len() {
        let c = s[i];
        if c == b'.' && !seen_point {
            seen_point = true;
            i += 1;
            continue;
        }
        let Some(d) = (c as char).to_digit(16) else { break };
        if mant >> 60 == 0 {
            mant = (mant << 4) | d as u64;
            if seen_point {
                exp -= 4;
            }
        } else {
            sticky |= d != 0;
            if !seen_point {
                exp += 4;
            }
        }
        i += 1;
    }
    if i < s.len() && (s[i] | 0x20) == b'p' {
        let mut k = i + 1;
        let mut eneg = false;
        if k < s.len() && (s[k] == b'+' || s[k] == b'-') {
            eneg = s[k] == b'-';
            k += 1;
        }
        let ds = k;
        let mut e: i64 = 0;
        while k < s.len() && s[k].is_ascii_digit() {
            e = (e * 10 + (s[k] - b'0') as i64).min(100_000);
            k += 1;
        }
        if k > ds {
            exp += if eneg { -e } else { e };
        }
    }
    if mant == 0 {
        return 0.0;
    }
    // Round the (up to 64-bit) significand to 53 bits, half to even, with the sticky bit.
    let lz = mant.leading_zeros() as i64;
    let top = mant << lz; // normalised: bit 63 set
    exp -= lz;
    let keep = top >> 11;
    let rem = top & 0x7ff;
    let half = 0x400;
    let mut m = keep;
    if rem > half || (rem == half && (sticky || keep & 1 == 1)) {
        m += 1;
    }
    let mut e2 = exp + 11;
    if m >> 53 != 0 {
        m >>= 1;
        e2 += 1;
    }
    let mut v = m as f64;
    // Scale by 2^e2 in exact power-of-two steps.
    while e2 > 0 {
        let step = e2.min(1000);
        v *= f64::from_bits(((1023 + step) as u64) << 52);
        e2 -= step;
        if v.is_infinite() {
            break;
        }
    }
    while e2 < 0 {
        let step = (-e2).min(1000);
        v *= f64::from_bits(((1023 - step) as u64) << 52);
        e2 += step;
        if v == 0.0 {
            break;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srs_line_fills_the_nine_slots_in_order() {
        // The line: POWER~SPEED~WATER_TEMPERATURE~RPM~TURBO~FUEL~OIL_T~OIL_P~VACUUM
        let v = parse(b"1~120~90~6500~1~55~100~4~0");
        assert_eq!(v, [1.0, 120.0, 90.0, 6500.0, 1.0, 55.0, 100.0, 4.0, 0.0]);
    }

    #[test]
    fn missing_fields_are_the_string_0() {
        assert_eq!(get_value(b"1~2", b'~', 2), b"0");
        assert_eq!(get_value(b"", b'~', 0), b"0");
        assert_eq!(parse(b"1~2"), [1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn get_value_edge_cases_follow_the_index_arithmetic() {
        // The last character is included even when it is the separator.
        assert_eq!(get_value(b"1~2~", b'~', 1), b"2~");
        assert_eq!(get_value(b"~5", b'~', 0), b"");
        assert_eq!(get_value(b"~5", b'~', 1), b"5");
        assert_eq!(get_value(b"abc", b'~', 0), b"abc");
        assert_eq!(get_value(b"a", b'~', 0), b"a");
        assert_eq!(get_value(b"a~~b", b'~', 1), b"");
    }

    #[test]
    fn value_is_cut_at_the_first_nul() {
        assert_eq!(parse(b"1~2\0~3"), [1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn atof_prefix_rules() {
        assert_eq!(to_float(b"  -12.5xyz"), -12.5);
        assert_eq!(to_float(b"abc"), 0.0);
        assert_eq!(to_float(b""), 0.0);
        assert_eq!(to_float(b"1e3"), 1000.0);
        assert_eq!(to_float(b"1e"), 1.0);
        assert_eq!(to_float(b"1e+"), 1.0);
        assert_eq!(to_float(b".5"), 0.5);
        assert_eq!(to_float(b"5."), 5.0);
        assert_eq!(to_float(b"."), 0.0);
        assert_eq!(to_float(b"0x10"), 16.0);
        assert_eq!(to_float(b"0x1.8p1"), 3.0);
        assert_eq!(to_float(b"0x"), 0.0);
        assert_eq!(to_float(b"0xg"), 0.0);
        assert!(to_float(b"inf").is_infinite());
        assert!(to_float(b"-Infinity") == f32::NEG_INFINITY);
        assert!(to_float(b"nan").is_nan());
        assert_eq!(to_float(b"2~"), 2.0);
        // double first, then float: 1e40 overflows the float conversion to inf.
        assert!(to_float(b"1e40").is_infinite());
        // round-to-nearest through the double
        assert_eq!(to_float(b"0.1"), 0.1f64 as f32);
    }
}

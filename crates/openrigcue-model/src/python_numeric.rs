//! Numeric text grammar and arithmetic of the legacy value model.

use crate::native_value::LegacyValue;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumericSum {
    Int(i128),
    Float(f64),
}

impl NumericSum {
    pub fn is_zero(self) -> bool {
        match self {
            Self::Int(value) => value == 0,
            Self::Float(value) => value == 0.0,
        }
    }
}

/// Summation used by IntelliTune and telemetry charts.
/// Integer accumulation remains integral; compensated floating accumulation ends at an integer wider than 32 bits.
pub fn sum_values(values: &[LegacyValue]) -> Result<NumericSum, String> {
    let mut integer = 0_i128;
    let mut floating: Option<f64> = None;
    let mut correction: f64 = 0.0;
    let mut fast = true;
    for value in values {
        match value {
            LegacyValue::Int(_) | LegacyValue::Bool(_) => {
                let value = match value {
                    LegacyValue::Int(value) => *value,
                    LegacyValue::Bool(value) => i64::from(*value),
                    _ => unreachable!(),
                };
                if let Some(total) = &mut floating {
                    if i32::try_from(value).is_err() {
                        if correction != 0.0 && correction.is_finite() {
                            *total += correction;
                        }
                        correction = 0.0;
                        fast = false;
                    }
                    *total += value as f64;
                } else {
                    integer = integer
                        .checked_add(i128::from(value))
                        .ok_or("OverflowError: integer sum exceeds supported range")?;
                }
            }
            LegacyValue::Float(value) => {
                if let Some(total) = &mut floating {
                    let updated = *total + value;
                    if fast {
                        correction += if total.abs() >= value.abs() {
                            (*total - updated) + value
                        } else {
                            (value - updated) + *total
                        };
                    }
                    *total = updated;
                } else {
                    floating = Some(integer as f64 + value);
                }
            }
            value => {
                return Err(format!(
                    "TypeError: unsupported operand type(s) for +: '{}' and '{}'",
                    if floating.is_some() { "float" } else { "int" },
                    value.type_name()
                ))
            }
        }
    }
    Ok(if let Some(mut total) = floating {
        if correction != 0.0 && correction.is_finite() {
            total += correction;
        }
        NumericSum::Float(total)
    } else {
        NumericSum::Int(integer)
    })
}

/// The digit limit of decimal text-to-integer conversion. Leading zeroes count as digits.
pub const DEFAULT_INT_MAX_STR_DIGITS: usize = 4300;

/// Unicode 15.1 decimal digits.
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0,
    0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620,
    0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x11066, 0x110F0, 0x11136, 0x111D0,
    0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950, 0x11C50, 0x11D50, 0x11DA0, 0x11F50,
    0x16A60, 0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950,
    0x1FBF0,
];

/// The value of a Unicode decimal digit.
pub fn decimal_digit(c: char) -> Option<u32> {
    let cp = u32::from(c);
    DECIMAL_ZEROS.iter().find(|&&zero| (zero..zero + 10).contains(&cp)).map(|zero| cp - zero)
}

pub fn decimal_ascii(text: &str) -> String {
    text.chars().map(|c| decimal_digit(c).map_or(c, |d| (b'0' + d as u8) as char)).collect()
}

/// Integer from text, bounded by this value model's i64 representation: surrounding whitespace, a sign, and
/// single underscores between digits are accepted.
pub fn parse_py_int(text: &str) -> Option<i64> {
    parse_py_int128(text).and_then(|value| i64::try_from(value).ok())
}

/// The decimal integer grammar without a machine-word representation bound.
pub fn parse_py_bigint(text: &str) -> Option<crate::native_value::BigInt> {
    python_int_text(text)?.parse().ok()
}

/// The same integer grammar with the wider range used by motion arithmetic.
pub fn parse_py_int128(text: &str) -> Option<i128> {
    python_int_text(text)?.parse().ok()
}

/// True division of this model's i128 operands, rounded once to f64.
/// None represents division by zero; integer operands cannot otherwise overflow f64.
pub fn int_true_divide(numerator: i128, denominator: i128) -> Option<f64> {
    use std::cmp::Ordering;
    if denominator == 0 {
        return None;
    }
    if numerator.unsigned_abs() <= (1_u128 << 53) && denominator.unsigned_abs() <= (1_u128 << 53) {
        return Some(numerator as f64 / denominator as f64);
    }
    let sign = u64::from((numerator < 0) != (denominator < 0)) << 63;
    if numerator == 0 {
        return Some(f64::from_bits(sign));
    }
    let (n, d) = (numerator.unsigned_abs(), denominator.unsigned_abs());
    let integer = n / d;
    let mut remainder = n % d;
    let (mut significand, mut exponent, round_up);
    if integer != 0 && 128 - integer.leading_zeros() >= 53 {
        let bits = 128 - integer.leading_zeros();
        exponent = bits as i32 - 1;
        let shift = bits - 53;
        significand = integer >> shift;
        round_up = if shift == 0 {
            match remainder.cmp(&(d - remainder)) {
                Ordering::Greater => true,
                Ordering::Equal => significand & 1 != 0,
                Ordering::Less => false,
            }
        } else {
            let lost = integer & ((1_u128 << shift) - 1);
            let half = 1_u128 << (shift - 1);
            lost > half || (lost == half && (remainder != 0 || significand & 1 != 0))
        };
    } else {
        fn next_bit(remainder: &mut u128, denominator: u128) -> u128 {
            // Avoid overflowing 2*remainder when the denominator is close to u128::MAX.
            if *remainder >= denominator - *remainder {
                *remainder -= denominator - *remainder;
                1
            } else {
                *remainder *= 2;
                0
            }
        }
        let remaining;
        if integer == 0 {
            exponent = 0;
            loop {
                exponent -= 1;
                if next_bit(&mut remainder, d) != 0 {
                    break;
                }
            }
            significand = 1;
            remaining = 52;
        } else {
            let bits = 128 - integer.leading_zeros();
            exponent = bits as i32 - 1;
            significand = integer;
            remaining = 53 - bits;
        }
        for _ in 0..remaining {
            significand = (significand << 1) | next_bit(&mut remainder, d);
        }
        round_up = match remainder.cmp(&(d - remainder)) {
            Ordering::Greater => true,
            Ordering::Equal => significand & 1 != 0,
            Ordering::Less => false,
        };
    }
    if round_up {
        significand += 1;
    }
    if significand == 1_u128 << 53 {
        significand >>= 1;
        exponent += 1;
    }
    Some(f64::from_bits(sign | (((exponent + 1023) as u64) << 52) | ((significand as u64) & ((1_u64 << 52) - 1))))
}

/// Validate decimal integer text and normalize Unicode digits/underscores without
/// applying a machine-integer width. The optional leading sign is retained.
pub fn integer_text(text: &str) -> Option<String> {
    let normalized = decimal_ascii(text.trim());
    let digits = normalized.strip_prefix(['+', '-']).unwrap_or(&normalized);
    if digits.is_empty() {
        return None;
    }
    let bytes = digits.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if *c == b'_' {
            if i == 0 || i + 1 == bytes.len() || !bytes[i - 1].is_ascii_digit() || !bytes[i + 1].is_ascii_digit() {
                return None;
            }
        } else if !c.is_ascii_digit() {
            return None;
        }
    }
    Some(normalized.replace('_', ""))
}

/// Normalize integer text, applying the default digit limit without a machine width.
/// Lexical-only callers that need to distinguish syntax and limit errors use [`integer_text`] instead.
pub fn python_int_text(text: &str) -> Option<String> {
    let normalized = integer_text(text)?;
    (normalized.trim_start_matches(['+', '-']).len() <= DEFAULT_INT_MAX_STR_DIGITS).then_some(normalized)
}

/// Float from text, including Unicode decimal digits and the inf/nan spellings.
pub fn parse_py_float(text: &str) -> Option<f64> {
    let normalized = decimal_ascii(text.trim());
    let bytes = normalized.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if *c == b'_'
            && (i == 0 || i + 1 == bytes.len() || !bytes[i - 1].is_ascii_digit() || !bytes[i + 1].is_ascii_digit())
        {
            return None;
        }
    }
    let cleaned = normalized.replace('_', "");
    let lower = cleaned.to_ascii_lowercase();
    let unsigned = lower.strip_prefix(['+', '-']).unwrap_or(&lower);
    if matches!(unsigned, "inf" | "infinity" | "nan") {
        return lower.parse().ok();
    }
    if !cleaned.chars().all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E')) {
        return None;
    }
    cleaned.parse().ok()
}

/// Strips surrounding whitespace, including the ASCII separator characters that the numeric
/// conversions do not accept unless a caller explicitly strips them first.
pub fn python_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_preserves_cpython_cancellation_and_mixed_integer_paths() {
        use LegacyValue::{Float as F, Int as I};
        // Compensated summation keeps small terms that plain addition would lose.
        assert_eq!(sum_values(&vec![F(0.1); 10]), Ok(NumericSum::Float(1.0)));
        assert_eq!(sum_values(&[F(1.0), F(1e101), F(1.0), F(-1e101)]), Ok(NumericSum::Float(2.0)));
        // Floats are compensated, but 32-bit integers are added
        // directly. A wider integer ends the compensated float loop.
        assert_eq!(sum_values(&[F(1e16), I(1), F(-1e16)]), Ok(NumericSum::Float(0.0)));
        assert_eq!(sum_values(&[F(1e16), F(1.0), F(-1e16)]), Ok(NumericSum::Float(1.0)));
        assert_eq!(
            sum_values(&[F(1e16), F(1.0), I(2_147_483_648), F(-1e16), F(1.0), F(-1.0)]),
            Ok(NumericSum::Float(2_147_483_648.0))
        );
        // A wide integer leaves the integer fast path; a later float still
        // enters the separate compensated floating loop.
        assert_eq!(
            sum_values(&[I(2_147_483_648), I(-2_147_483_648), F(1e16), F(1.0), F(-1e16)]),
            Ok(NumericSum::Float(1.0))
        );
        assert_eq!(sum_values(&[I(i64::MAX), I(i64::MAX)]), Ok(NumericSum::Int(i128::from(i64::MAX) * 2)));
    }

    #[test]
    fn wide_integer_accumulation_still_enters_the_compensated_float_loop() {
        use LegacyValue::{Float as F, Int as I};
        for prefix in [vec![I(2_147_483_648), I(-2_147_483_648)], vec![I(i64::MAX), I(1), I(-i64::MAX), I(-1)]] {
            let values = [prefix, vec![F(1e16), F(1.0), F(-1e16)]].concat();
            assert_eq!(sum_values(&values), Ok(NumericSum::Float(1.0)));
        }
    }

    #[test]
    fn sum_preserves_nonfinite_results_and_initial_integer_zero() {
        use LegacyValue::Float as F;
        let NumericSum::Float(zero) = sum_values(&[F(-0.0)]).unwrap() else { panic!("float sum expected") };
        assert_eq!(zero.to_bits(), 0.0f64.to_bits());
        assert_eq!(sum_values(&[F(f64::INFINITY), F(f64::INFINITY)]), Ok(NumericSum::Float(f64::INFINITY)));
        assert_eq!(sum_values(&[F(1e308), F(1e308)]), Ok(NumericSum::Float(f64::INFINITY)));
        for values in [[F(f64::INFINITY), F(f64::NEG_INFINITY)], [F(0.0), F(f64::NAN)]] {
            assert!(matches!(sum_values(&values), Ok(NumericSum::Float(value)) if value.is_nan()));
        }
        assert_eq!(
            sum_values(&[LegacyValue::Int(1), LegacyValue::Str("2".into())]).unwrap_err(),
            "TypeError: unsupported operand type(s) for +: 'int' and 'str'"
        );
        assert_eq!(
            sum_values(&[F(1.0), LegacyValue::Str("2".into())]).unwrap_err(),
            "TypeError: unsupported operand type(s) for +: 'float' and 'str'"
        );
    }

    #[test]
    fn float_text_rounds_at_binary64_extremes_and_retains_signed_zero() {
        for (text, bits) in [
            ("4.9406564584124654e-324", 1),
            ("2.4703282292062327e-324", 0),
            ("2.4703282292062328e-324", 1),
            ("-2.4703282292062327e-324", 1_u64 << 63),
            ("1.7976931348623157e308", f64::MAX.to_bits()),
            ("1.7976931348623159e308", f64::INFINITY.to_bits()),
            ("-0.0", 1_u64 << 63),
            (" \u{0661}_\u{0662}.\u{0665}e-1 ", 1.25f64.to_bits()),
        ] {
            assert_eq!(parse_py_float(text).map(f64::to_bits), Some(bits), "{text:?}");
        }
        for text in ["\u{1c}1.0", "1.0\u{1f}", "1_.0", "1._0", "1e_2", "0x1p0"] {
            assert_eq!(parse_py_float(text), None, "{text:?}");
        }
    }

    #[test]
    fn decimal_int_conversion_counts_leading_zeroes_but_not_signs_or_separators() {
        let within = format!("{}7", "0".repeat(4299));
        assert_eq!(parse_py_int(&within), Some(7));
        assert_eq!(parse_py_int128(&within), Some(7));
        let over = format!("0{within}");
        assert_eq!(parse_py_int(&over), None);
        assert_eq!(parse_py_int128(&over), None);
        let unicode = format!("  -{}٧\n", "٠_".repeat(4299));
        assert_eq!(parse_py_int(&unicode), Some(-7));
        assert_eq!(parse_py_int(&unicode.replacen('٧', "٠_٧", 1)), None);
        assert_eq!(parse_py_float(&over), Some(7.0), "float conversion has no decimal-int digit limit");
    }
}

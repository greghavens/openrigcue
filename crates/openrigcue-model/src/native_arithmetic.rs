//! Arithmetic over the legacy values shared by live consumers.
//! No text-to-number conversion occurs until an explicit conversion is requested.

use crate::native_value::LegacyValue;
use crate::python_compare::{native_equal, numeric_cmp};
use crate::python_string::PythonString;
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{One, ToPrimitive, Zero};
use std::cmp::Ordering;

fn kind(value: &LegacyValue) -> &str {
    value.qualified_type_name()
}

fn unsupported(left: &LegacyValue, op: &str, right: &LegacyValue) -> String {
    format!("TypeError: unsupported operand type(s) for {op}: '{}' and '{}'", kind(left), kind(right))
}
fn memory_error() -> String {
    "MemoryError".into()
}

/// Correctly round an integer ratio once, including subnormals and signed zero.
/// Scaling is bounded to the binary64 significand/exponent range, rather than
/// converting either operand independently and losing cancellation or overflow.
pub fn integer_true_divide(left: &BigInt, right: &BigInt) -> Result<f64, String> {
    if right.is_zero() {
        return Err("ZeroDivisionError: division by zero".into());
    }
    let negative = (left.sign() == Sign::Minus) != (right.sign() == Sign::Minus);
    let sign = u64::from(negative) << 63;
    if left.is_zero() {
        return Ok(f64::from_bits(sign));
    }
    let a = left.magnitude();
    let b = right.magnitude();
    let mut exponent = i128::from(a.bits()) - i128::from(b.bits());
    if exponent > 1024 {
        return Err("OverflowError: integer division result too large for a float".into());
    }
    if exponent < -1075 {
        return Ok(f64::from_bits(sign));
    }
    let below = if exponent >= 0 { a < &(b << exponent as usize) } else { &(a << (-exponent) as usize) < b };
    if below {
        exponent -= 1;
    }
    if exponent > 1023 {
        return Err("OverflowError: integer division result too large for a float".into());
    }
    if exponent < -1075 {
        return Ok(f64::from_bits(sign));
    }
    let scale = if exponent >= -1022 { 52 - exponent } else { 1074 };
    let (numerator, denominator) =
        if scale >= 0 { (a << scale as usize, b.clone()) } else { (a.clone(), b << (-scale) as usize) };
    let mut quotient = &numerator / &denominator;
    let remainder = numerator % &denominator;
    let twice = remainder << 1usize;
    if twice > denominator || (twice == denominator && (&quotient & BigUint::one()) == BigUint::one()) {
        quotient += 1u8;
    }
    let mut significand = quotient.to_u64().expect("scaled binary64 significand");
    let bits = if exponent >= -1022 {
        if significand == (1u64 << 53) {
            significand >>= 1;
            exponent += 1;
        }
        if exponent > 1023 {
            return Err("OverflowError: integer division result too large for a float".into());
        }
        (((exponent + 1023) as u64) << 52) | (significand - (1u64 << 52))
    } else {
        significand
    };
    Ok(f64::from_bits(sign | bits))
}

fn numeric(left: &LegacyValue, right: &LegacyValue, operator: &str) -> Option<Result<LegacyValue, String>> {
    if let (Some(a), Some(b)) = (left.integer(), right.integer()) {
        return Some(match operator {
            "+" => Ok(LegacyValue::from_integer(a + b)),
            "-" => Ok(LegacyValue::from_integer(a - b)),
            "*" => Ok(LegacyValue::from_integer(a * b)),
            "/" => integer_true_divide(&a, &b).map(LegacyValue::from_float),
            _ => unreachable!(),
        });
    }
    if !(left.integer().is_some() || matches!(left, LegacyValue::Float(_)))
        || !(right.integer().is_some() || matches!(right, LegacyValue::Float(_)))
    {
        return None;
    }
    Some((|| {
        let a = left.py_as_double()?;
        let b = right.py_as_double()?;
        Ok(LegacyValue::from_float(match operator {
            "+" => a + b,
            "-" => a - b,
            "*" => a * b,
            "/" => {
                if b == 0.0 {
                    return Err("ZeroDivisionError: float division by zero".into());
                }
                a / b
            }
            _ => unreachable!(),
        }))
    })())
}

fn concatenate<T: Clone>(left: &[T], right: &[T]) -> Result<Vec<T>, String> {
    let size = left.len().checked_add(right.len()).filter(|n| *n <= isize::MAX as usize).ok_or_else(memory_error)?;
    let mut result = Vec::new();
    result.try_reserve_exact(size).map_err(|_| memory_error())?;
    result.extend_from_slice(left);
    result.extend_from_slice(right);
    Ok(result)
}
fn repeated<T: Clone>(sequence: &[T], count: isize, message: &str) -> Result<Vec<T>, String> {
    if count <= 0 {
        return Ok(Vec::new());
    }
    let size = sequence
        .len()
        .checked_mul(count as usize)
        .filter(|n| *n <= isize::MAX as usize)
        .ok_or_else(|| message.to_owned())?;
    let mut result = Vec::new();
    result.try_reserve_exact(size).map_err(|_| memory_error())?;
    if sequence.is_empty() {
        return Ok(result);
    }
    for _ in 0..count {
        result.extend_from_slice(sequence);
    }
    Ok(result)
}
fn sequence(value: &LegacyValue) -> bool {
    value.python_string().is_some()
        || matches!(
            value,
            LegacyValue::Bytes(_) | LegacyValue::ByteArray(_) | LegacyValue::List(_) | LegacyValue::Tuple(_)
        )
}
fn repeat(value: &LegacyValue, multiplier: &LegacyValue) -> Result<LegacyValue, String> {
    let integer = multiplier
        .integer()
        .ok_or_else(|| format!("TypeError: can't multiply sequence by non-int of type '{}'", kind(multiplier)))?;
    let count =
        integer.to_isize().ok_or_else(|| "OverflowError: cannot fit 'int' into an index-sized integer".to_owned())?;
    if let Some(text) = value.python_string() {
        let points = repeated(text.codepoints(), count, "OverflowError: repeated string is too long")?;
        return PythonString::from_codepoints(points).map(LegacyValue::from_python_string);
    }
    match value {
        LegacyValue::Bytes(v) => {
            repeated(v, count, "OverflowError: repeated bytes are too long").map(LegacyValue::Bytes)
        }
        LegacyValue::ByteArray(v) => repeated(v, count, "MemoryError").map(LegacyValue::ByteArray),
        LegacyValue::List(v) => repeated(v, count, "MemoryError").map(LegacyValue::List),
        LegacyValue::Tuple(v) => repeated(v, count, "MemoryError").map(LegacyValue::Tuple),
        _ => unreachable!("sequence checked"),
    }
}

impl LegacyValue {
    /// Implicit real-number conversion for arithmetic; unlike an explicit float conversion, text is rejected.
    pub fn py_as_double(&self) -> Result<f64, String> {
        let _operation = crate::native_object::native_operation();
        if let Self::Object(value) = self {
            return value.resolved().py_as_double();
        }
        if let Self::Float(value) = self {
            return Ok(*value);
        }
        if self.integer().is_some() {
            return self.py_float_native();
        }
        Err(format!("TypeError: must be real number, not {}", kind(self)))
    }
    pub fn py_add(&self, other: &Self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if matches!(self, Self::Object(_)) || matches!(other, Self::Object(_)) {
            return self.resolved().py_add(&other.resolved());
        }
        if let Some(value) = datetime_arithmetic(self, other, "+") {
            return value;
        }
        if let Some(result) = numeric(self, other, "+") {
            return result;
        }
        if let Some(left) = self.python_string() {
            let right = other
                .python_string()
                .ok_or_else(|| format!("TypeError: can only concatenate str (not \"{}\") to str", kind(other)))?;
            return PythonString::from_codepoints(concatenate(left.codepoints(), right.codepoints())?)
                .map(Self::from_python_string);
        }
        match (self, other) {
            (Self::Bytes(a), other) if other.buffer_bytes().is_some() => {
                concatenate(a, &other.buffer_bytes().expect("buffer")).map(Self::Bytes)
            }
            (Self::ByteArray(a), other) if other.buffer_bytes().is_some() => {
                concatenate(a, &other.buffer_bytes().expect("buffer")).map(Self::ByteArray)
            }
            (Self::ByteArray(_), other) => Err(format!("TypeError: can't concat {} to bytearray", kind(other))),
            (Self::List(a), Self::List(b)) => concatenate(a, b).map(Self::List),
            (Self::Tuple(a), Self::Tuple(b)) => concatenate(a, b).map(Self::Tuple),
            (Self::Bytes(_), b) => Err(format!("TypeError: can't concat {} to bytes", kind(b))),
            (Self::List(_), b) => Err(format!("TypeError: can only concatenate list (not \"{}\") to list", kind(b))),
            (Self::Tuple(_), b) => Err(format!("TypeError: can only concatenate tuple (not \"{}\") to tuple", kind(b))),
            _ => Err(unsupported(self, "+", other)),
        }
    }
    pub fn py_sub(&self, other: &Self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if matches!(self, Self::Object(_)) || matches!(other, Self::Object(_)) {
            return self.resolved().py_sub(&other.resolved());
        }
        if let Some(value) = datetime_arithmetic(self, other, "-") {
            return value;
        }
        if let Some(result) = numeric(self, other, "-") {
            return result;
        }
        match (self, other) {
            (Self::Set(a) | Self::FrozenSet(a), Self::Set(b) | Self::FrozenSet(b)) => {
                let values = a.iter().filter(|v| !b.iter().any(|other| native_equal(v, other))).cloned().collect();
                Ok(if matches!(self, Self::Set(_)) { Self::Set(values) } else { Self::FrozenSet(values) })
            }
            _ => Err(unsupported(self, "-", other)),
        }
    }
    pub fn py_mul(&self, other: &Self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if matches!(self, Self::Object(_)) || matches!(other, Self::Object(_)) {
            return self.resolved().py_mul(&other.resolved());
        }
        if let Some(value) = datetime_arithmetic(self, other, "*") {
            return value;
        }
        if let Some(result) = numeric(self, other, "*") {
            return result;
        }
        if sequence(self) {
            return repeat(self, other);
        }
        if sequence(other) {
            return repeat(other, self);
        }
        Err(unsupported(self, "*", other))
    }
    pub fn py_truediv(&self, other: &Self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if matches!(self, Self::Object(_)) || matches!(other, Self::Object(_)) {
            return self.resolved().py_truediv(&other.resolved());
        }
        if let Some(value) = datetime_arithmetic(self, other, "/") {
            return value;
        }
        numeric(self, other, "/").unwrap_or_else(|| Err(unsupported(self, "/", other)))
    }
    pub fn py_neg(&self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if let Self::TimeDelta(value) = self {
            return timedelta_from_microseconds(-delta_micros(value));
        }
        if let Self::Object(value) = self {
            return value.resolved().py_neg();
        }
        if let Some(value) = self.integer() {
            return Ok(Self::from_integer(-value));
        }
        if let Self::Float(value) = self {
            return Ok(Self::from_float(-value));
        }
        Err(format!("TypeError: bad operand type for unary -: '{}'", kind(self)))
    }
    pub fn py_abs(&self) -> Result<Self, String> {
        let _operation = crate::native_object::native_operation();
        if let Self::TimeDelta(value) = self {
            let value = delta_micros(value);
            return timedelta_from_microseconds(if value.sign() == Sign::Minus { -value } else { value });
        }
        if let Self::Object(value) = self {
            return value.resolved().py_abs();
        }
        if let Some(value) = self.integer() {
            return Ok(Self::from_integer(if value.sign() == Sign::Minus { -value } else { value }));
        }
        if let Self::Float(value) = self {
            return Ok(Self::from_float(value.abs()));
        }
        Err(format!("TypeError: bad operand type for abs(): '{}'", kind(self)))
    }
    fn py_compare(&self, other: &Self, operator: &str, holds: impl FnOnce(Ordering) -> bool) -> Result<bool, String> {
        let _operation = crate::native_object::native_operation();
        numeric_cmp(self, other).map(|order| order.is_some_and(holds)).map_err(|error| {
            if error.starts_with("RecursionError:") {
                error
            } else {
                format!(
                    "TypeError: '{operator}' not supported between instances of '{}' and '{}'",
                    kind(self),
                    kind(other)
                )
            }
        })
    }
    pub fn py_lt(&self, other: &Self) -> Result<bool, String> {
        self.py_compare(other, "<", |o| o == Ordering::Less)
    }
    pub fn py_le(&self, other: &Self) -> Result<bool, String> {
        self.py_compare(other, "<=", |o| o != Ordering::Greater)
    }
    pub fn py_gt(&self, other: &Self) -> Result<bool, String> {
        self.py_compare(other, ">", |o| o == Ordering::Greater)
    }
    pub fn py_ge(&self, other: &Self) -> Result<bool, String> {
        self.py_compare(other, ">=", |o| o != Ordering::Less)
    }
}

const DAY_MICROS: i64 = 86_400_000_000;
fn delta_micros(value: &crate::native_value::TimeDeltaParts) -> BigInt {
    BigInt::from(value.days) * DAY_MICROS + BigInt::from(value.seconds) * 1_000_000 + value.microseconds
}
fn floor_divmod(value: BigInt, divisor: &BigInt) -> (BigInt, BigInt) {
    let mut q = &value / divisor;
    let mut r = value % divisor;
    if r.sign() == Sign::Minus {
        q -= 1;
        r += divisor;
    }
    (q, r)
}
/// Bounded, normalized duration constructor applied after integer arithmetic.
pub fn timedelta_from_microseconds(value: BigInt) -> Result<LegacyValue, String> {
    let (days, rem) = floor_divmod(value, &BigInt::from(DAY_MICROS));
    let days = days.to_i32().ok_or_else(|| "OverflowError: Python int too large to convert to C int".to_owned())?;
    if !(-999_999_999..=999_999_999).contains(&days) {
        return Err(format!("OverflowError: days={days}; must have magnitude <= 999999999"));
    }
    let rem = rem.to_u64().expect("normalized day remainder");
    Ok(LegacyValue::TimeDelta(crate::native_value::TimeDeltaParts {
        days,
        seconds: (rem / 1_000_000) as u32,
        microseconds: (rem % 1_000_000) as u32,
    }))
}
fn nearest_divide(value: BigInt, divisor: BigInt) -> Result<BigInt, String> {
    if divisor.is_zero() {
        return Err("ZeroDivisionError: integer division or modulo by zero".into());
    }
    let negative = (value.sign() == Sign::Minus) != (divisor.sign() == Sign::Minus);
    let a = value.magnitude();
    let b = divisor.magnitude();
    let mut q = a / b;
    let r = a % b;
    if (&r << 1usize) > *b || ((&r << 1usize) == *b && (&q & BigUint::one()) == BigUint::one()) {
        q += 1u8;
    }
    Ok(BigInt::from_biguint(if negative { Sign::Minus } else { Sign::Plus }, q))
}
fn float_ratio(value: f64) -> Result<(BigInt, BigInt), String> {
    if value.is_infinite() {
        return Err("OverflowError: cannot convert Infinity to integer ratio".into());
    }
    if value.is_nan() {
        return Err("ValueError: cannot convert NaN to integer ratio".into());
    }
    let bits = value.to_bits();
    let raw = (bits >> 52) & 0x7ff;
    let mantissa = (bits & ((1u64 << 52) - 1)) | if raw == 0 { 0 } else { 1u64 << 52 };
    let exponent = if raw == 0 { -1074 } else { raw as i32 - 1023 - 52 };
    let mut n = BigInt::from(mantissa);
    let mut d = BigInt::one();
    if exponent >= 0 {
        n <<= exponent as usize;
    } else {
        d <<= (-exponent) as usize;
    }
    if value.is_sign_negative() {
        n = -n;
    }
    Ok((n, d))
}
fn year_days(year: i64) -> i64 {
    let previous = year - 1;
    previous * 365 + previous / 4 - previous / 100 + previous / 400
}
fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
fn month_days(year: i64, month: u8) -> i64 {
    match month {
        2 => {
            if leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
fn date_micros(value: &crate::native_value::DateTimeParts) -> BigInt {
    let mut day = year_days(i64::from(value.year)) + i64::from(value.day) - 1;
    for month in 1..value.month {
        day += month_days(i64::from(value.year), month);
    }
    BigInt::from(day) * DAY_MICROS
        + BigInt::from(u64::from(value.hour) * 3_600 + u64::from(value.minute) * 60 + u64::from(value.second))
            * 1_000_000
        + value.microsecond
}
fn date_from_micros(value: BigInt) -> Result<LegacyValue, String> {
    let (day, rem) = floor_divmod(value, &BigInt::from(DAY_MICROS));
    let day = day
        .to_i64()
        .filter(|day| *day >= 0 && *day < year_days(10_000))
        .ok_or_else(|| "OverflowError: date value out of range".to_owned())?;
    let (mut lower, mut upper) = (1i64, 10_000i64);
    while lower + 1 < upper {
        let middle = (lower + upper) / 2;
        if year_days(middle) <= day {
            lower = middle;
        } else {
            upper = middle;
        }
    }
    let mut rest = day - year_days(lower);
    let mut month = 1;
    while rest >= month_days(lower, month) {
        rest -= month_days(lower, month);
        month += 1;
    }
    let rem = rem.to_u64().expect("day remainder");
    let seconds = rem / 1_000_000;
    Ok(LegacyValue::DateTime(crate::native_value::DateTimeParts {
        year: lower as u16,
        month,
        day: (rest + 1) as u8,
        hour: (seconds / 3_600) as u8,
        minute: (seconds / 60 % 60) as u8,
        second: (seconds % 60) as u8,
        microsecond: (rem % 1_000_000) as u32,
        fold: false,
    }))
}
fn datetime_arithmetic(left: &LegacyValue, right: &LegacyValue, op: &str) -> Option<Result<LegacyValue, String>> {
    use LegacyValue::{DateTime, Float, TimeDelta};
    match (left, right, op) {
        (DateTime(a), DateTime(b), "-") => Some(timedelta_from_microseconds(date_micros(a) - date_micros(b))),
        (DateTime(a), TimeDelta(b), "+") | (TimeDelta(b), DateTime(a), "+") => {
            Some(date_from_micros(date_micros(a) + delta_micros(b)))
        }
        (DateTime(a), TimeDelta(b), "-") => Some(date_from_micros(date_micros(a) - delta_micros(b))),
        (TimeDelta(a), TimeDelta(b), "+") => Some(timedelta_from_microseconds(delta_micros(a) + delta_micros(b))),
        (TimeDelta(a), TimeDelta(b), "-") => Some(timedelta_from_microseconds(delta_micros(a) - delta_micros(b))),
        (TimeDelta(a), TimeDelta(b), "/") => {
            Some(integer_true_divide(&delta_micros(a), &delta_micros(b)).map(LegacyValue::from_float))
        }
        (TimeDelta(delta), Float(float), "*") | (Float(float), TimeDelta(delta), "*") => Some((|| {
            let (n, d) = float_ratio(*float)?;
            timedelta_from_microseconds(nearest_divide(delta_micros(delta) * n, d)?)
        })()),
        (TimeDelta(delta), Float(float), "/") => Some((|| {
            let (n, d) = float_ratio(*float)?;
            timedelta_from_microseconds(nearest_divide(delta_micros(delta) * d, n)?)
        })()),
        (TimeDelta(delta), integer, "*") | (integer, TimeDelta(delta), "*") if integer.integer().is_some() => {
            Some(timedelta_from_microseconds(delta_micros(delta) * integer.integer().expect("int")))
        }
        (TimeDelta(delta), integer, "/") if integer.integer().is_some() => Some(
            nearest_divide(delta_micros(delta), integer.integer().expect("int")).and_then(timedelta_from_microseconds),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_object::NativeObject;
    use crate::native_value::{DateTimeParts, TimeDeltaParts};
    fn integer(value: BigInt) -> LegacyValue {
        LegacyValue::from_integer(value)
    }
    #[test]
    fn exact_native_integer_family_retains_values_beyond_machine_words() {
        let n = BigInt::one() << 2048usize;
        assert_eq!(integer(n.clone()).py_add(&integer(-n.clone())).unwrap(), LegacyValue::Int(0));
        assert_eq!(integer(n.clone()).py_sub(&LegacyValue::Int(1)).unwrap().integer(), Some(&n - 1));
        assert_eq!(integer(n.clone()).py_mul(&LegacyValue::Int(-2)).unwrap().integer(), Some(-&n * 2));
        assert_eq!(integer(-n.clone()).py_abs().unwrap().integer(), Some(n.clone()));
        assert_eq!(integer(n.clone()).py_truediv(&integer(n.clone())).unwrap(), LegacyValue::Float(1.0));
        assert_eq!(
            integer(n.clone()).py_truediv(&LegacyValue::Float(0.0)).unwrap_err(),
            "OverflowError: int too large to convert to float"
        );
        assert!(integer(n).py_gt(&LegacyValue::Float(f64::MAX)).unwrap());
    }
    #[test]
    fn exact_ratio_rounding_subnormal_signed_zero_and_overflow() {
        for (a, b, bits) in [
            (9_007_199_254_740_993i64, 3i64, 0x4325_5555_5555_5556u64),
            (9_007_199_254_740_993, 7, 0x4312_4924_9249_2493),
        ] {
            assert_eq!(integer_true_divide(&a.into(), &b.into()).unwrap().to_bits(), bits);
        }
        let denominator = BigInt::one() << 1075usize;
        assert_eq!(integer_true_divide(&BigInt::one(), &denominator).unwrap().to_bits(), 0);
        assert_eq!(integer_true_divide(&BigInt::from(3), &denominator).unwrap().to_bits(), 2);
        assert_eq!(integer_true_divide(&BigInt::from(-1), &denominator).unwrap().to_bits(), 1u64 << 63);
        let huge = BigInt::one() << 1024usize;
        assert_eq!(
            integer_true_divide(&huge, &BigInt::one()).unwrap_err(),
            "OverflowError: integer division result too large for a float"
        );
        assert_eq!(integer_true_divide(&BigInt::zero(), &BigInt::from(-1)).unwrap().to_bits(), 1u64 << 63);
    }
    #[test]
    fn object_scalars_sequences_and_repeated_children_keep_native_semantics() {
        let scalar = LegacyValue::Object(NativeObject::new(LegacyValue::Float(2.5)));
        assert_eq!(scalar.py_as_double(), Ok(2.5));
        assert_eq!(scalar.py_add(&LegacyValue::Int(2)), Ok(LegacyValue::Float(4.5)));
        let child = LegacyValue::Object(NativeObject::new(LegacyValue::List(vec![LegacyValue::Int(1)])));
        let value = LegacyValue::Object(NativeObject::new(LegacyValue::List(vec![child.clone()])));
        let LegacyValue::List(repeated) = value.py_mul(&LegacyValue::Int(2)).unwrap() else { panic!("list repeat") };
        assert!(repeated[0].same_object(&child) && repeated[1].same_object(&child));
        let mut changed = child.clone();
        changed.assign_index(0, LegacyValue::Int(8)).unwrap();
        assert_eq!(repeated[1].get_index(0), Ok(LegacyValue::Int(8)));
        assert_eq!(
            LegacyValue::Str("x".into()).py_mul(&LegacyValue::Float(0.0)).unwrap_err(),
            "TypeError: can't multiply sequence by non-int of type 'float'"
        );
        assert_eq!(
            LegacyValue::Tuple(vec![]).py_mul(&integer(BigInt::one() << 128usize)).unwrap_err(),
            "OverflowError: cannot fit 'int' into an index-sized integer"
        );
    }
    #[test]
    fn unicode_and_bytes_operations_preserve_contents_and_surrogates() {
        let raw = LegacyValue::from_python_string(PythonString::from_codepoints(vec![0xd800, 0x61]).unwrap());
        assert_eq!(
            raw.py_mul(&LegacyValue::Int(2)).unwrap().python_string().unwrap().codepoints(),
            [0xd800, 0x61, 0xd800, 0x61]
        );
        assert_eq!(
            raw.py_add(&LegacyValue::Str("b".into())).unwrap().python_string().unwrap().codepoints(),
            [0xd800, 0x61, 0x62]
        );
        assert_eq!(LegacyValue::Bytes(vec![255]).py_mul(&LegacyValue::Int(-1)), Ok(LegacyValue::Bytes(vec![])));
        assert_eq!(
            LegacyValue::Bytes(vec![255]).py_add(&LegacyValue::Bytes(vec![0])),
            Ok(LegacyValue::Bytes(vec![255, 0]))
        );
    }
    fn date(year: u16, month: u8, day: u8) -> LegacyValue {
        LegacyValue::DateTime(DateTimeParts {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            microsecond: 0,
            fold: false,
        })
    }
    #[test]
    fn datetime_subtraction_and_normalized_delta_arithmetic() {
        let day = LegacyValue::TimeDelta(TimeDeltaParts { days: 1, seconds: 0, microseconds: 0 });
        assert_eq!(date(2024, 3, 1).py_sub(&date(2024, 2, 29)), Ok(day.clone()));
        assert_eq!(date(2024, 2, 28).py_add(&day), Ok(date(2024, 2, 29)));
        let micros = LegacyValue::TimeDelta(TimeDeltaParts { days: 0, seconds: 0, microseconds: 3 });
        assert_eq!(
            micros.py_mul(&LegacyValue::Float(0.5)),
            Ok(LegacyValue::TimeDelta(TimeDeltaParts { days: 0, seconds: 0, microseconds: 2 }))
        );
        assert_eq!(
            micros.py_truediv(&LegacyValue::Int(2)),
            Ok(LegacyValue::TimeDelta(TimeDeltaParts { days: 0, seconds: 0, microseconds: 2 }))
        );
        assert_eq!(
            micros.py_neg(),
            Ok(LegacyValue::TimeDelta(TimeDeltaParts { days: -1, seconds: 86_399, microseconds: 999_997 }))
        );
        assert_eq!(day.py_truediv(&day), Ok(LegacyValue::Float(1.0)));
        assert_eq!(date(1, 1, 1).py_sub(&day).unwrap_err(), "OverflowError: date value out of range");
        assert_eq!(
            micros.py_mul(&LegacyValue::Float(f64::NAN)).unwrap_err(),
            "ValueError: cannot convert NaN to integer ratio"
        );
    }
    #[test]
    fn arithmetic_recursion_errors_are_not_reclassified_as_type_errors() {
        let mut a = LegacyValue::Object(NativeObject::new(LegacyValue::List(vec![LegacyValue::None])));
        let mut b = LegacyValue::Object(NativeObject::new(LegacyValue::List(vec![LegacyValue::None])));
        a.assign_index(0, a.clone()).unwrap();
        b.assign_index(0, b.clone()).unwrap();
        assert!(a.py_lt(&b).unwrap_err().starts_with("RecursionError:"));
    }
    #[test]
    fn native_buffer_arithmetic_keeps_bytearray_kind_and_rejects_view_repeat() {
        use crate::native_value::NativeBufferView;
        use std::sync::Arc;
        let view =
            LegacyValue::MemoryView(Arc::new(NativeBufferView { buffer: LegacyValue::Bytes(vec![2]), readonly: true }));
        assert_eq!(LegacyValue::Bytes(vec![1]).py_add(&view), Ok(LegacyValue::Bytes(vec![1, 2])));
        assert_eq!(LegacyValue::ByteArray(vec![1]).py_add(&view), Ok(LegacyValue::ByteArray(vec![1, 2])));
        assert_eq!(
            LegacyValue::ByteArray(vec![1]).py_mul(&LegacyValue::Int(2)),
            Ok(LegacyValue::ByteArray(vec![1, 1]))
        );
        assert_eq!(
            view.py_mul(&LegacyValue::Int(0)).unwrap_err(),
            "TypeError: unsupported operand type(s) for *: 'memoryview' and 'int'"
        );
    }
}

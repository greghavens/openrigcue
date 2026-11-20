//! Operations of the motion drivers on the shared motor list of dynamic values.
//!
//! The list a driver receives is the caller's `LegacyValue::Object(List)` (the same object the motion
//! pipeline keeps as its stream and its previous frame), its slots integers of any width. A driver reads a
//! slot, writes it back into the list (visible to
//! every holder at once), and computes with the model's arbitrary-width arithmetic. Every width limit is the one
//! of the packing step that needs it (fixed-width integers, 16-bit words, 64-bit floats of the
//! rescale), reported as that step's error.

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};
use openrigcue_model::native_value::LegacyValue;
use openrigcue_model::python_compare::native_equal_checked;

use super::{MotionError, PyFrame};

/// A configuration value as a driver reads it: the raw value, or the
/// error (`Class: message`) of its lookup. Conversions to an integer or a boolean happen at the
/// driver step that needs them.
pub type NativeSetting = Result<LegacyValue, String>;

/// The frame of the rescale, which converts each argument to a 64-bit float on entry.
pub const FUNCTIONS_MAP: PyFrame = PyFrame::new("backend/functions.py", "map", 204);

/// A driver error with the text `Class: message`.
pub fn raised(detail: impl Into<String>) -> MotionError {
    MotionError::Native(detail.into())
}

/// An integer value.
pub fn int(value: i64) -> LegacyValue {
    LegacyValue::Int(value)
}

/// An integer value of any width.
pub fn big(value: BigInt) -> LegacyValue {
    LegacyValue::from_integer(value)
}

/// The length of the motor list where a driver needs it to report a short list; `0` for a value without a length.
fn length(list: &LegacyValue) -> usize {
    list.resolved().py_len().unwrap_or(0)
}

/// Slot `index` of the motor list. A list too short is an index error (kept as
/// [`MotionError::TooFewMotors`]); any other failure keeps its own class.
pub fn item(list: &LegacyValue, index: usize) -> Result<LegacyValue, MotionError> {
    list.get_index(index).map_err(|detail| index_error(list, index, detail))
}

/// The slot of the motor list for an index that is itself a dynamic integer (negative counts from the end).
pub fn item_at(list: &LegacyValue, index: &LegacyValue) -> Result<LegacyValue, MotionError> {
    list.lookup_owned(index).map_err(|detail| {
        let needed = index.integer().and_then(|i| i.abs().to_usize()).unwrap_or(usize::MAX - 1);
        index_error(list, needed, detail)
    })
}

fn index_error(list: &LegacyValue, index: usize, detail: String) -> MotionError {
    if detail.starts_with("IndexError: list index out of range") {
        MotionError::TooFewMotors { needed: index.saturating_add(1), got: length(list) }
    } else {
        raised(detail)
    }
}

/// Stores `value` in slot `index` of the motor list, published to every holder of the list at once.
pub fn assign(list: &mut LegacyValue, index: usize, value: LegacyValue) -> Result<(), MotionError> {
    list.assign_index(index, value).map_err(raised)
}

fn arithmetic(result: Result<LegacyValue, String>) -> Result<LegacyValue, MotionError> {
    result.map_err(raised)
}

/// `a + b`.
pub fn add(a: &LegacyValue, b: &LegacyValue) -> Result<LegacyValue, MotionError> {
    arithmetic(a.py_add(b))
}

/// `a - b`.
pub fn sub(a: &LegacyValue, b: &LegacyValue) -> Result<LegacyValue, MotionError> {
    arithmetic(a.py_sub(b))
}

/// `a * b`.
pub fn mul(a: &LegacyValue, b: &LegacyValue) -> Result<LegacyValue, MotionError> {
    arithmetic(a.py_mul(b))
}

/// `a / b`.
pub fn truediv(a: &LegacyValue, b: &LegacyValue) -> Result<LegacyValue, MotionError> {
    arithmetic(a.py_truediv(b))
}

/// `-a`.
pub fn neg(a: &LegacyValue) -> Result<LegacyValue, MotionError> {
    arithmetic(a.py_neg())
}

/// `a < b`.
pub fn lt(a: &LegacyValue, b: &LegacyValue) -> Result<bool, MotionError> {
    a.py_lt(b).map_err(raised)
}

/// `a > b`.
pub fn gt(a: &LegacyValue, b: &LegacyValue) -> Result<bool, MotionError> {
    a.py_gt(b).map_err(raised)
}

/// `a <= b`.
pub fn le(a: &LegacyValue, b: &LegacyValue) -> Result<bool, MotionError> {
    a.py_le(b).map_err(raised)
}

/// `a >= b`.
pub fn ge(a: &LegacyValue, b: &LegacyValue) -> Result<bool, MotionError> {
    a.py_ge(b).map_err(raised)
}

/// `a == b`.
pub fn eq(a: &LegacyValue, b: &LegacyValue) -> Result<bool, MotionError> {
    native_equal_checked(a, b).map_err(raised)
}

/// The minimum of `a` and `b`: the first of the smallest (`b` replaces `a` only when `b < a`).
pub fn min(a: &LegacyValue, b: &LegacyValue) -> Result<LegacyValue, MotionError> {
    Ok(if lt(b, a)? { b.clone() } else { a.clone() })
}

/// The maximum of `a` and `b`: the first of the largest (`b` replaces `a` only when `b > a`).
pub fn max(a: &LegacyValue, b: &LegacyValue) -> Result<LegacyValue, MotionError> {
    Ok(if gt(b, a)? { b.clone() } else { a.clone() })
}

/// `value` converted to an integer value.
pub fn to_int(value: &LegacyValue) -> Result<LegacyValue, MotionError> {
    value.py_integer().map(big).map_err(raised)
}

/// `value` converted to an integer, as the integer itself.
pub fn to_bigint(value: &LegacyValue) -> Result<BigInt, MotionError> {
    value.py_integer().map_err(raised)
}

/// An argument as a 64-bit float: integers of any width that fit, floats;
/// text is rejected.
pub fn double(value: &LegacyValue) -> Result<f64, MotionError> {
    value.py_as_double().map_err(raised)
}

/// The rescale of `x` from `in_min..in_max` to `out_min..out_max` on dynamic arguments: each argument is converted
/// to a 64-bit float in order on entry (an integer too wide for one is an overflow error there), then the clamped
/// linear map of [`super::driver_support::map_clamped`].
pub fn map(args: [&LegacyValue; 5]) -> Result<f64, MotionError> {
    let mut doubles = [0.0; 5];
    for (slot, arg) in doubles.iter_mut().zip(args) {
        *slot = double(arg).map_err(|e| e.in_frame(FUNCTIONS_MAP))?;
    }
    let [x, a, b, c, d] = doubles;
    Ok(super::driver_support::map_clamped(x, a, b, c, d))
}

/// A float as an integer: truncation toward zero, at any width.
pub fn trunc_float(value: f64) -> Result<LegacyValue, MotionError> {
    to_int(&LegacyValue::from_float(value))
}

/// The float result of a rescale as a dynamic value.
pub fn float(value: f64) -> LegacyValue {
    LegacyValue::from_float(value)
}

fn type_name(value: &LegacyValue) -> &'static str {
    value.qualified_type_name()
}

/// The integer of a value where only an integer is accepted: integers and booleans; a float or text is
/// a type error.
pub fn index(value: &LegacyValue) -> Result<BigInt, MotionError> {
    value
        .integer()
        .ok_or_else(|| raised(format!("TypeError: '{}' object cannot be interpreted as an integer", type_name(value))))
}

/// The low 16 bits of any integer; a float and other non-integers are
/// type errors.
pub fn c_uint16(value: &LegacyValue) -> Result<u16, MotionError> {
    if matches!(value.resolved(), LegacyValue::Float(_)) {
        return Err(raised("TypeError: int expected instead of float"));
    }
    let integer = index(value)?;
    let modulus = BigInt::from(65_536);
    let mut low = integer % &modulus;
    if low.is_negative() {
        low += &modulus;
    }
    Ok(low.to_u16().expect("reduced modulo 65536"))
}

/// The low 32 bits of any integer, as a signed value (same conversions as [`c_uint16`]).
pub fn c_int(value: &LegacyValue) -> Result<i32, MotionError> {
    if matches!(value.resolved(), LegacyValue::Float(_)) {
        return Err(raised("TypeError: int expected instead of float"));
    }
    let integer = index(value)?;
    let modulus = BigInt::from(1u64 << 32);
    let mut low = integer % &modulus;
    if low.is_negative() {
        low += &modulus;
    }
    Ok(low.to_u32().expect("reduced modulo 2**32") as i32)
}

/// The unsigned bytes of `value` in the given length and byte order: an overflow error for a negative value
/// or one too wide; an attribute error for a non-integer.
pub fn to_bytes(value: &LegacyValue, length: usize, big_endian: bool) -> Result<Vec<u8>, MotionError> {
    let integer = value
        .integer()
        .ok_or_else(|| raised(format!("AttributeError: '{}' object has no attribute 'to_bytes'", type_name(value))))?;
    if integer.is_negative() {
        return Err(raised("OverflowError: can't convert negative int to unsigned"));
    }
    let (_, mut bytes) = integer.to_bytes_be();
    if integer.is_zero() {
        bytes.clear();
    }
    if bytes.len() > length {
        return Err(raised("OverflowError: int too big to convert"));
    }
    let mut out = vec![0u8; length - bytes.len()];
    out.extend_from_slice(&bytes);
    if !big_endian {
        out.reverse();
    }
    Ok(out)
}

/// `(value >> shift) & mask` (`value & mask` for a zero shift) of an integer; any other operand is the operator's
/// type error. The result is the low byte the mask keeps.
pub fn shift_right_and(value: &LegacyValue, shift: usize, mask: u8) -> Result<u8, MotionError> {
    let operator = if shift == 0 { "&" } else { ">>" };
    let integer = value.integer().filter(|_| !matches!(value.resolved(), LegacyValue::Float(_))).ok_or_else(|| {
        raised(format!("TypeError: unsupported operand type(s) for {operator}: '{}' and 'int'", type_name(value)))
    })?;
    let shifted = integer >> shift;
    let masked = shifted & BigInt::from(mask);
    Ok(masked.to_u8().expect("masked to one byte"))
}

/// Lower-case hexadecimal of an integer of any width, zero-padded to `width` so that the sign counts
/// toward `width`; any other value is a type error.
pub fn percent_hex(value: &LegacyValue, width: usize) -> Result<String, MotionError> {
    if matches!(value.resolved(), LegacyValue::Float(_)) || value.integer().is_none() {
        return Err(raised(format!("TypeError: %x format: an integer is required, not {}", type_name(value))));
    }
    let integer = value.integer().expect("checked above");
    let digits = format!("{:x}", integer.magnitude());
    let sign = if integer.is_negative() { "-" } else { "" };
    let pad = width.saturating_sub(sign.len() + digits.len());
    Ok(format!("{sign}{}{digits}", "0".repeat(pad)))
}

/// `value` as a signed 32-bit integer, big-endian (little-endian when `big_endian` is false).
pub fn pack_i32(value: &LegacyValue, big_endian: bool) -> Result<[u8; 4], MotionError> {
    let integer = struct_integer(value)?;
    let packed = integer
        .to_i32()
        .ok_or_else(|| raised("struct.error: 'l' format requires -2147483648 <= number <= 2147483647"))?;
    Ok(if big_endian { packed.to_be_bytes() } else { packed.to_le_bytes() })
}

/// `value` as an unsigned 16-bit integer, in either byte order.
pub fn pack_u16(value: &LegacyValue, big_endian: bool) -> Result<[u8; 2], MotionError> {
    let integer = struct_integer(value)?;
    let packed = integer.to_u16().ok_or_else(|| raised("struct.error: 'H' format requires 0 <= number <= 65535"))?;
    Ok(if big_endian { packed.to_be_bytes() } else { packed.to_le_bytes() })
}

/// `value` as a signed 16-bit integer, in either byte order.
pub fn pack_i16(value: &LegacyValue, big_endian: bool) -> Result<[u8; 2], MotionError> {
    let integer = struct_integer(value)?;
    let packed =
        integer.to_i16().ok_or_else(|| raised("struct.error: 'h' format requires -32768 <= number <= 32767"))?;
    Ok(if big_endian { packed.to_be_bytes() } else { packed.to_le_bytes() })
}

/// The integer a fixed-width packing takes: an integer or boolean; anything else is
/// a packing error.
fn struct_integer(value: &LegacyValue) -> Result<BigInt, MotionError> {
    value.integer().ok_or_else(|| raised("struct.error: required argument is not an integer"))
}

/// The text of a number, as messages print it.
pub fn str_of(value: &LegacyValue) -> String {
    value.resolved().py_str().unwrap_or_else(|| openrigcue_model::python_repr::repr(value))
}

/// The text of a value where a message is built: the model's checked conversion, so an integer
/// beyond the 4300-digit decimal limit is a value error there.
pub fn str_checked(value: &LegacyValue) -> Result<String, MotionError> {
    let text = openrigcue_model::python_repr::str_native(value).map_err(raised)?;
    Ok(text.scalar_string().unwrap_or_else(|| text.repr()))
}

/// The value as an `i64`, where one exists (a convenience projection; never used to decide a
/// branch).
pub fn as_i64(value: &LegacyValue) -> Option<i64> {
    value.integer().and_then(|v| v.to_i64())
}

/// A dynamic list of the given integers (the entry of the `&[i64]` driver APIs).
pub fn list_of(values: &[i64]) -> LegacyValue {
    LegacyValue::List(values.iter().copied().map(LegacyValue::Int).collect())
}

/// The slots of a dynamic list read back as `i64` for the `&[i64]` APIs; a slot that holds no `i64` keeps
/// the caller's value.
pub fn read_back(list: &LegacyValue, into: &mut [i64]) {
    for (index, slot) in into.iter_mut().enumerate() {
        if let Some(value) = list.get_index(index).ok().as_ref().and_then(as_i64) {
            *slot = value;
        }
    }
}

/// The slots of a dynamic list as `i64`s, for `take_edited_frame`.
pub fn edited_vec(list: &LegacyValue, original: &[i64]) -> Vec<i64> {
    let mut out = original.to_vec();
    read_back(list, &mut out);
    out
}

/// Runs a driver's dynamic-value frame path for an `&[i64]` caller: the integers as a fresh list, and that
/// list as the frame path left it.
pub fn compat<R>(
    motors: &[i64],
    call: impl FnOnce(&mut LegacyValue) -> Result<R, MotionError>,
) -> (Result<R, MotionError>, LegacyValue) {
    let mut list = list_of(motors);
    let result = call(&mut list);
    (result, list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_model::native_object::NativeObject;

    fn wide() -> LegacyValue {
        big(BigInt::from(1) << 80)
    }

    #[test]
    fn slot_reads_and_writes_reach_every_alias() {
        let shared = LegacyValue::Object(NativeObject::new(LegacyValue::List(vec![int(1), int(2)])));
        let mut handle = shared.clone();
        assign(&mut handle, 1, wide()).unwrap();
        assert_eq!(item(&shared, 1).unwrap(), wide());
        let error = item(&shared, 2).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "IndexError: list index out of range");
        let error = assign(&mut handle, 5, int(0)).unwrap_err();
        assert_eq!(error.python_str(), "list assignment index out of range");
    }

    #[test]
    fn packing_raises_the_packing_call_exception() {
        assert_eq!(to_bytes(&int(258), 2, true).unwrap(), vec![1, 2]);
        assert_eq!(to_bytes(&int(65_536), 2, true).unwrap_err().python_str(), "int too big to convert");
        assert_eq!(to_bytes(&int(-1), 2, true).unwrap_err().python_str(), "can't convert negative int to unsigned");
        assert_eq!(to_bytes(&wide(), 2, true).unwrap_err().python_str(), "int too big to convert");
        assert_eq!(c_uint16(&wide()).unwrap(), 0);
        assert_eq!(c_uint16(&big((BigInt::from(1) << 80) + 7)).unwrap(), 7);
        assert_eq!(c_uint16(&int(-1)).unwrap(), 65_535);
        assert_eq!(c_uint16(&float(1.0)).unwrap_err().python_str(), "int expected instead of float");
        assert_eq!(
            pack_i32(&wide(), true).unwrap_err().exception_line().unwrap(),
            "struct.error: 'l' format requires -2147483648 <= number <= 2147483647"
        );
        assert_eq!(pack_i32(&int(-2), true).unwrap(), [0xFF, 0xFF, 0xFF, 0xFE]);
    }

    #[test]
    fn map_converts_its_arguments_to_doubles_on_entry() {
        let huge = big(BigInt::from(1) << 1100);
        let error = map([&huge, &int(0), &int(1), &int(0), &int(1)]).unwrap_err();
        assert_eq!(error.exception_line().unwrap(), "OverflowError: int too large to convert to float");
        assert_eq!(error.frames(), &[FUNCTIONS_MAP]);
        assert_eq!(map([&wide(), &int(0), &int(65_535), &int(0), &int(100)]).unwrap(), 100.0);
        assert_eq!(map([&int(-5), &int(0), &int(10), &int(0), &int(100)]).unwrap(), 0.0);
    }

    #[test]
    fn str_applies_the_decimal_digit_limit() {
        assert_eq!(str_checked(&int(-5000)).unwrap(), "-5000");
        let wide = big(num_traits::pow(BigInt::from(10), 4300));
        let error = str_checked(&wide).unwrap_err();
        assert!(error.exception_line().unwrap().starts_with("ValueError: Exceeds the limit (4300 digits)"), "{error}");
        assert_eq!(str_checked(&big(num_traits::pow(BigInt::from(10), 4299))).unwrap().len(), 4300);
    }

    #[test]
    fn builtins_keep_python_order_and_width() {
        assert_eq!(min(&int(3), &int(3)).unwrap(), int(3));
        assert_eq!(max(&LegacyValue::Bool(true), &int(1)).unwrap(), LegacyValue::Bool(true));
        assert_eq!(sub(&int(65_536), &wide()).unwrap(), big(BigInt::from(65_536) - (BigInt::from(1) << 80)));
        assert_eq!(to_int(&float(-2.7)).unwrap(), int(-2));
    }
}

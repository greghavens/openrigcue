//! Builtin comparisons shared by telemetry derivation and output consumers.
//!
//! Integer/float comparisons retain the integer's precision. The represented container values use
//! their builtin recursive comparisons; custom objects and alias-sensitive behavior are not represented.

use crate::native_value::BigInt;
use crate::native_value::LegacyValue;
use num_traits::FromPrimitive;
use std::cmp::Ordering;

pub fn numeric_cmp(left: &LegacyValue, right: &LegacyValue) -> Result<Option<Ordering>, String> {
    let _operation = crate::native_object::native_operation();
    if matches!(left, LegacyValue::Object(_)) || matches!(right, LegacyValue::Object(_)) {
        return object_comparison(left, right, || numeric_cmp(&left.resolved(), &right.resolved()));
    }
    fn integer(value: &LegacyValue) -> Option<BigInt> {
        value.integer()
    }
    fn int_float(integer: BigInt, float: f64) -> Option<Ordering> {
        if float.is_nan() {
            return None;
        }
        if float == f64::NEG_INFINITY {
            return Some(Ordering::Greater);
        }
        if float == f64::INFINITY {
            return Some(Ordering::Less);
        }
        match integer.cmp(&BigInt::from_f64(float.trunc()).expect("finite truncated double")) {
            Ordering::Equal => 0.0_f64.partial_cmp(&float.fract()),
            ordering => Some(ordering),
        }
    }
    match (integer(left), integer(right), left, right) {
        (Some(left), Some(right), _, _) => Ok(Some(left.cmp(&right))),
        (Some(left), _, _, LegacyValue::Float(right)) => Ok(int_float(left, *right)),
        (_, Some(right), LegacyValue::Float(left), _) => Ok(int_float(right, *left).map(Ordering::reverse)),
        (_, _, LegacyValue::Float(left), LegacyValue::Float(right)) => Ok(left.partial_cmp(right)),
        (_, _, LegacyValue::Str(left), LegacyValue::Str(right)) => Ok(Some(left.cmp(right))),
        (_, _, left, right) if left.python_string().is_some() && right.python_string().is_some() => {
            Ok(Some(left.python_string().expect("string").cmp(&right.python_string().expect("string"))))
        }
        (_, _, LegacyValue::Bytes(left), LegacyValue::Bytes(right)) => Ok(Some(left.cmp(right))),
        (
            _,
            _,
            LegacyValue::Bytes(left) | LegacyValue::ByteArray(left),
            LegacyValue::Bytes(right) | LegacyValue::ByteArray(right),
        ) => Ok(Some(left.cmp(right))),
        (_, _, LegacyValue::DateTime(left), LegacyValue::DateTime(right)) => Ok(Some(
            crate::native_value::DateTimeParts { fold: false, ..*left }
                .cmp(&crate::native_value::DateTimeParts { fold: false, ..*right }),
        )),
        (_, _, LegacyValue::TimeDelta(left), LegacyValue::TimeDelta(right)) => Ok(Some(left.cmp(right))),
        (_, _, LegacyValue::List(left), LegacyValue::List(right))
        | (_, _, LegacyValue::Tuple(left), LegacyValue::Tuple(right)) => {
            for (left, right) in left.iter().zip(right) {
                if !left.same_object(right) && !native_equal_checked(left, right)? {
                    return numeric_cmp(left, right);
                }
            }
            Ok(Some(left.len().cmp(&right.len())))
        }
        (
            _,
            _,
            LegacyValue::Set(left) | LegacyValue::FrozenSet(left),
            LegacyValue::Set(right) | LegacyValue::FrozenSet(right),
        ) => {
            let left_subset = left
                .iter()
                .all(|value| right.iter().any(|other| value.same_object(other) || native_equal(value, other)));
            let right_subset = right
                .iter()
                .all(|value| left.iter().any(|other| value.same_object(other) || native_equal(value, other)));
            Ok(match (left_subset, right_subset) {
                (true, true) => Some(Ordering::Equal),
                (true, false) => Some(Ordering::Less),
                (false, true) => Some(Ordering::Greater),
                (false, false) => None,
            })
        }
        _ => Err(format!("comparison between {} and {} failed", left.type_name(), right.type_name())),
    }
}

/// Equality for the represented builtin values, retaining numeric cross-kind comparisons.
/// Custom objects, identity-sensitive containers and unsupported key aliases are outside this model.
pub fn native_equal(left: &LegacyValue, right: &LegacyValue) -> bool {
    native_equal_checked(left, right).unwrap_or(false)
}

/// Value equality with its recursion/error boundary retained. Container
/// element lookup additionally short-circuits an identical object.
pub fn native_equal_checked(left: &LegacyValue, right: &LegacyValue) -> Result<bool, String> {
    let _operation = crate::native_object::native_operation();
    if matches!(left, LegacyValue::Object(_)) || matches!(right, LegacyValue::Object(_)) {
        if left.same_object(right)
            && matches!(
                left.resolved(),
                LegacyValue::List(_)
                    | LegacyValue::Tuple(_)
                    | LegacyValue::Dict(_)
                    | LegacyValue::Set(_)
                    | LegacyValue::FrozenSet(_)
            )
        {
            return Ok(true);
        }
        return object_comparison(left, right, || native_equal_checked(&left.resolved(), &right.resolved()));
    }
    Ok(match (left, right) {
        (LegacyValue::List(a), LegacyValue::List(b)) | (LegacyValue::Tuple(a), LegacyValue::Tuple(b)) => {
            if a.len() != b.len() {
                false
            } else {
                let mut equal = true;
                for (a, b) in a.iter().zip(b) {
                    if !a.same_object(b) && !native_equal_checked(a, b)? {
                        equal = false;
                        break;
                    }
                }
                equal
            }
        }
        (LegacyValue::Dict(a), LegacyValue::Dict(b)) => {
            if a.len() != b.len() {
                false
            } else {
                let mut equal = true;
                for (k, v) in a.iter() {
                    let Some(other) = b.get_key(k) else {
                        equal = false;
                        break;
                    };
                    if !v.same_object(other) && !native_equal_checked(v, other)? {
                        equal = false;
                        break;
                    }
                }
                equal
            }
        }
        _ => native_equal_plain(left, right),
    })
}

fn native_equal_plain(left: &LegacyValue, right: &LegacyValue) -> bool {
    match (left, right) {
        (left, right) if left.buffer_bytes().is_some() && right.buffer_bytes().is_some() => {
            left.buffer_bytes() == right.buffer_bytes()
        }
        (LegacyValue::DateTime(left), LegacyValue::DateTime(right)) => {
            // The represented datetimes are naive: builtin equality ignores fold.
            let left = crate::native_value::DateTimeParts { fold: false, ..*left };
            let right = crate::native_value::DateTimeParts { fold: false, ..*right };
            left == right
        }
        (LegacyValue::List(left), LegacyValue::List(right)) | (LegacyValue::Tuple(left), LegacyValue::Tuple(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(left, right)| native_equal(left, right))
        }
        (LegacyValue::Dict(left), LegacyValue::Dict(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| right.get_key(key).is_some_and(|other| native_equal(value, other)))
        }
        (LegacyValue::Set(_) | LegacyValue::FrozenSet(_), LegacyValue::Set(_) | LegacyValue::FrozenSet(_)) => {
            numeric_cmp(left, right).ok().flatten() == Some(Ordering::Equal)
        }
        (left, right) if left.python_string().is_some() && right.python_string().is_some() => {
            left.python_string() == right.python_string()
        }
        _ if left.number().is_some() && right.number().is_some() => {
            numeric_cmp(left, right).ok().flatten() == Some(Ordering::Equal)
        }
        _ => left == right,
    }
}

thread_local! {static COMPARING:std::cell::RefCell<std::collections::BTreeSet<(usize,usize)>>=const{std::cell::RefCell::new(std::collections::BTreeSet::new())};}
fn object_comparison<T>(
    left: &LegacyValue,
    right: &LegacyValue,
    run: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let id = |v: &LegacyValue| {
        if let LegacyValue::Object(o) = v {
            o.id() << 1
        } else {
            (v as *const LegacyValue as usize) | 1
        }
    };
    let pair = (id(left), id(right));
    if !COMPARING.with(|stack| stack.borrow_mut().insert(pair)) {
        return Err("RecursionError: maximum recursion depth exceeded in comparison".into());
    }
    let result = run();
    COMPARING.with(|stack| {
        stack.borrow_mut().remove(&pair);
    });
    result
}

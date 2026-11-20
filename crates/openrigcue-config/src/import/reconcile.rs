//! Applies changes made through a typed view without rewriting untouched stored values.
//!
//! The stored dictionaries retain int/float, missing/null and insertion-order distinctions that a typed
//! projection cannot represent. Only differences between the old and new projections reach the raw value.

use super::value::LegacyValue;

/// Applies only actual typed-view changes to a retained canonical value.
pub fn reconcile(target: &mut LegacyValue, before: &LegacyValue, after: &LegacyValue) {
    apply(target, before, after, true);
}

/// [`reconcile`] for a reader that does not own `target`: a shared dictionary a change falls into is left as it
/// is, and `target` takes a copy of it with the change. Unchanged members remain the shared values, and without a
/// change `target` is not touched.
pub fn reconcile_detached(target: &mut LegacyValue, before: &LegacyValue, after: &LegacyValue) {
    apply(target, before, after, false);
}

fn apply(target: &mut LegacyValue, before: &LegacyValue, after: &LegacyValue, in_place: bool) {
    if unchanged(before, after) {
        return;
    }
    if let LegacyValue::Object(object) = target {
        let mut value = object.resolved();
        if matches!((&value, before, after), (LegacyValue::Dict(_), LegacyValue::Dict(_), LegacyValue::Dict(_))) {
            apply(&mut value, before, after, in_place);
            if in_place {
                object.replace(value);
            } else {
                *target = value;
            }
            return;
        }
    }
    if let (LegacyValue::Dict(target), LegacyValue::Dict(before), LegacyValue::Dict(after)) =
        (&mut *target, before, after)
    {
        for key in before.keys() {
            if after.get_key(key).is_none() {
                // The configuration stores use string keys. Preserve unrelated, non-string raw keys.
                if let super::value::Key::Str(key) = key {
                    target.remove(key);
                }
            }
        }
        for (key, after) in after.iter() {
            match (before.get_key(key), target.get_key(key).cloned()) {
                (Some(before), Some(mut value)) => {
                    apply(&mut value, before, after, in_place);
                    target.insert(key.clone(), value);
                }
                (Some(before), None) if unchanged(before, after) => {}
                _ => target.insert(key.clone(), after.clone()),
            }
        }
    } else {
        *target = after.clone();
    }
}

// This compares two snapshots of the same typed projection, not raw-value equality.
// Equal NaN bits mean no edit; different zero signs are a real assignment.
pub(crate) fn unchanged(before: &LegacyValue, after: &LegacyValue) -> bool {
    match (before, after) {
        (LegacyValue::Float(a), LegacyValue::Float(b)) => a.to_bits() == b.to_bits(),
        (LegacyValue::List(a), LegacyValue::List(b))
        | (LegacyValue::Tuple(a), LegacyValue::Tuple(b))
        | (LegacyValue::Set(a), LegacyValue::Set(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| unchanged(a, b))
        }
        (LegacyValue::Dict(a), LegacyValue::Dict(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|((ak, av), (bk, bv))| ak == bk && unchanged(av, bv))
        }
        _ => before == after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicitly_changed_float_zero_sign_reaches_the_retained_value() {
        let mut target = LegacyValue::Float(0.0);
        reconcile(&mut target, &LegacyValue::Float(0.0), &LegacyValue::Float(-0.0));
        let LegacyValue::Float(value) = target else { panic!("float expected") };
        assert_eq!(value.to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn unchanged_nan_projection_does_not_materialize_a_missing_native_field() {
        let nan = LegacyValue::Float(f64::from_bits(0x7ff8_0000_0000_0042));
        let before = LegacyValue::Dict([("missing".into(), nan.clone())].into_iter().collect());
        let after = before.clone();
        let mut target = LegacyValue::Dict(Default::default());
        reconcile(&mut target, &before, &after);
        assert!(target.as_dict().unwrap().is_empty(), "unchanged projection must preserve missing native key");
    }
}

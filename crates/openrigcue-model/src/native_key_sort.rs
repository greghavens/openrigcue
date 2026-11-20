//! Sorting the unique builtin keys of a legacy dictionary.
//!
//! The run-recognition, binary-insertion and merge-scheduling algorithm is the timsort
//! design (PSF License 2, LICENSES/CPython-3.13.txt). The existing IntelliTune
//! sorter supplies the run-recognition adaptation. Galloping and both merge directions
//! use the value model's less-than comparison, including unordered NaNs and partially ordered
//! sets; incompatible key kinds fail at the first comparison that meets them.

use crate::native_value::Key;
use std::cmp::Ordering;
mod merge;

fn less(left: &Key, right: &Key) -> Result<bool, String> {
    left.native_cmp(right).map(|order| order == Ordering::Less).map_err(|error| {
        if error.starts_with("RecursionError:") {
            error
        } else {
            format!(
                "'<' not supported between instances of '{}' and '{}'",
                left.qualified_type_name(),
                right.qualified_type_name()
            )
        }
    })
}

fn run<T>(values: &mut [T], key: &impl Fn(&T) -> &Key) -> Result<usize, String> {
    let mut n = 1;
    while n < values.len() && !less(key(&values[n]), key(&values[n - 1]))? {
        n += 1;
    }
    if n == values.len() {
        return Ok(n);
    }
    if n > 1 {
        if less(key(&values[0]), key(&values[n - 1]))? {
            return Ok(n);
        }
        values[..n].reverse();
    }
    n += 1;
    let mut equal = 0;
    while n < values.len() {
        if less(key(&values[n]), key(&values[n - 1]))? {
            if equal != 0 {
                values[n - equal - 1..n].reverse();
                equal = 0;
            }
        } else if less(key(&values[n - 1]), key(&values[n]))? {
            break;
        } else {
            equal += 1;
        }
        n += 1;
    }
    if equal != 0 {
        values[n - equal - 1..n].reverse();
    }
    values[..n].reverse();
    while n < values.len() && !less(key(&values[n]), key(&values[n - 1]))? {
        n += 1;
    }
    Ok(n)
}

struct Run {
    start: usize,
    len: usize,
    power: u32,
}

// The first binary digit at which the adjacent run midpoints differ.
fn power(previous: &Run, next_len: usize, total: usize) -> u32 {
    let (mut a, total) = ((2 * previous.start as u128) + previous.len as u128, total as u128);
    let mut b = a + previous.len as u128 + next_len as u128;
    let mut depth = 0;
    loop {
        depth += 1;
        if a >= total {
            a -= total;
            b -= total;
        } else if b >= total {
            return depth;
        }
        a *= 2;
        b *= 2;
    }
}

fn merge<T: Clone>(
    values: &mut [T],
    pending: &mut Vec<Run>,
    index: usize,
    min_gallop: &mut usize,
    key: &impl Fn(&T) -> &Key,
) -> Result<(), String> {
    let start = pending[index].start;
    let middle = pending[index + 1].start;
    let end = middle + pending[index + 1].len;
    merge::adjacent(values, start, middle - start, end - middle, min_gallop, key)?;
    pending[index].len += pending[index + 1].len;
    pending.remove(index + 1);
    Ok(())
}

/// Sort entries from one legacy Dict, retaining builtin ordering and the first comparison error.
/// The input must contain unique represented keys, as guaranteed by Dict assignment semantics.
pub fn sort_by_key<T: Clone>(values: &mut [T], key: impl Fn(&T) -> &Key) -> Result<(), String> {
    if values.len() < 2 {
        return Ok(());
    }
    let mut minrun = values.len();
    let mut remainder = 0;
    while minrun >= 64 {
        remainder |= minrun & 1;
        minrun >>= 1;
    }
    minrun += remainder;
    let mut pending: Vec<Run> = Vec::new();
    let mut min_gallop = 7;
    let mut start = 0;
    while start < values.len() {
        let sorted = run(&mut values[start..], &key)?;
        let len = sorted.max(minrun.min(values.len() - start));
        for index in start + sorted..start + len {
            let (mut low, mut high) = (start, index);
            while low < high {
                let middle = (low + high) / 2;
                if less(key(&values[index]), key(&values[middle]))? {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            values[low..=index].rotate_right(1);
        }
        if let Some(previous) = pending.last() {
            let power = power(previous, len, values.len());
            while pending.len() > 1 && pending[pending.len() - 2].power > power {
                let index = pending.len() - 2;
                merge(values, &mut pending, index, &mut min_gallop, &key)?;
            }
            pending.last_mut().unwrap().power = power;
        }
        pending.push(Run { start, len, power: 0 });
        start += len;
    }
    while pending.len() > 1 {
        let mut index = pending.len() - 2;
        if index > 0 && pending[index - 1].len < pending[index + 1].len {
            index -= 1;
        }
        merge(values, &mut pending, index, &mut min_gallop, &key)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_order_and_error_operands_survive_run_and_merge_boundaries() {
        let mut numeric: Vec<_> = (0..129)
            .map(|i| match (i * 73) % 129 {
                0 => Key::Bool(false),
                1 => Key::Bool(true),
                value => Key::Int(value),
            })
            .collect();
        sort_by_key(&mut numeric, |key| key).unwrap();
        assert_eq!(
            numeric.iter().map(|key| key.to_value().py_int().unwrap()).collect::<Vec<_>>(),
            (0..129).collect::<Vec<_>>()
        );
        let mut mixed = vec![
            Key::Int(9_007_199_254_740_993),
            Key::float(9_007_199_254_740_992.0).unwrap(),
            Key::Bool(true),
            Key::float(f64::NEG_INFINITY).unwrap(),
            Key::float(f64::INFINITY).unwrap(),
            Key::float(-0.0).unwrap(),
        ];
        sort_by_key(&mut mixed, |key| key).unwrap();
        assert_eq!(
            mixed.iter().map(Key::to_text).collect::<Vec<_>>(),
            ["-inf", "-0.0", "True", "9007199254740992.0", "9007199254740993", "inf"]
        );
        let mut merge_boundary = vec![Key::Bool(true), Key::Int(0), Key::Int(3), Key::Int(2)];
        merge_boundary.extend((4..48).map(Key::Int));
        merge_boundary.extend((0..48).map(|i| Key::from(format!("Car {i:02}"))));
        for (mut keys, comparison) in [
            (vec![Key::Int(0), Key::Bool(true), Key::from("Car")], "'str' and 'bool'"),
            (vec![Key::Bool(true), Key::Int(0), Key::Int(2), Key::from("Car")], "'str' and 'int'"),
            (vec![Key::Bool(true), Key::Int(3), Key::Int(2), Key::from("Car")], "'str' and 'int'"),
            (merge_boundary, "'str' and 'int'"),
        ] {
            assert_eq!(
                sort_by_key(&mut keys, |key| key).unwrap_err(),
                format!("'<' not supported between instances of {comparison}")
            );
        }
    }

    #[test]
    fn unordered_native_keys_keep_the_source_natural_run() {
        use crate::native_value::{LegacyValue, NativeKey};
        let nan = Key::from_value(&LegacyValue::from_float(f64::NAN)).unwrap();
        let mut values = vec![Key::Int(2), nan.clone(), Key::Int(1)];
        sort_by_key(&mut values, |value| value).unwrap();
        assert_eq!(values, vec![Key::Int(2), nan, Key::Int(1)]);
        let a = Key::Extra(NativeKey::FrozenSet(vec![Key::Int(2)]));
        let b = Key::Extra(NativeKey::FrozenSet(vec![Key::Int(1)]));
        let both = Key::Extra(NativeKey::FrozenSet(vec![Key::Int(1), Key::Int(2)]));
        let mut values = vec![a.clone(), b.clone(), both.clone()];
        sort_by_key(&mut values, |value| value).unwrap();
        assert_eq!(values, vec![a, b, both]);
    }
}

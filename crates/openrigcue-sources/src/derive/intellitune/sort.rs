//! Short-list sort with a fixed comparison order, including NaNs. IntelliTune caps its lists at 31.
//! Adapted from CPython3.13.11 Objects/listobject.c count_run/binarysort (PSF License2).
//! https://github.com/python/cpython/blob/v3.13.11/Objects/listobject.c
//! License: LICENSES/CPython-3.13.txt. Larger externally injected lists remain outside this runtime invariant.
use super::numeric_cmp;
use openrigcue_model::native_value::LegacyValue;
use std::cmp::Ordering;
fn less(a: &LegacyValue, b: &LegacyValue) -> Result<bool, String> {
    Ok(numeric_cmp(a, b)? == Some(Ordering::Less))
}
fn run(values: &mut [LegacyValue]) -> Result<usize, String> {
    let size = values.len();
    let mut n = 1;
    while n < size && !less(&values[n], &values[n - 1])? {
        n += 1;
    }
    if n == size {
        return Ok(n);
    }
    if n > 1 {
        if less(&values[0], &values[n - 1])? {
            return Ok(n);
        }
        values[..n].reverse();
    }
    n += 1;
    let mut equal = 0;
    while n < size {
        if less(&values[n], &values[n - 1])? {
            if equal != 0 {
                values[n - equal - 1..n].reverse();
                equal = 0;
            }
        } else if less(&values[n - 1], &values[n])? {
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
    while n < size && !less(&values[n], &values[n - 1])? {
        n += 1;
    }
    Ok(n)
}
pub(super) fn descending(values: &mut [LegacyValue]) -> Result<(), String> {
    if values.len() < 2 {
        return Ok(());
    }
    // A descending sort: reverse before and after a stable ascending sort.
    values.reverse();
    let sorted = run(values)?;
    for index in sorted..values.len() {
        let (mut lo, mut hi) = (0, index);
        while lo < hi {
            let middle = (lo + hi) / 2;
            if less(&values[index], &values[middle])? {
                hi = middle;
            } else {
                lo = middle + 1;
            }
        }
        values[lo..=index].rotate_right(1);
    }
    values.reverse();
    Ok(())
}

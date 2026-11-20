//! Fallible timsort merge/gallop control order (PSF License 2).
//! The temporary run holds actual cloned entries, retaining object identities.
use super::{less, Key};

fn gallop<T>(pivot: &T, values: &[T], hint: usize, right: bool, key: &impl Fn(&T) -> &Key) -> Result<usize, String> {
    let mut offset = 1isize;
    let mut last = 0isize;
    let hint = hint as isize;
    let n = values.len() as isize;
    let before = if right {
        less(key(pivot), key(&values[hint as usize]))?
    } else {
        !less(key(&values[hint as usize]), key(pivot))?
    };
    if before {
        let max = hint + 1;
        while offset < max {
            let matched = if right {
                less(key(pivot), key(&values[(hint - offset) as usize]))?
            } else {
                !less(key(&values[(hint - offset) as usize]), key(pivot))?
            };
            if !matched {
                break;
            }
            last = offset;
            offset = offset.saturating_mul(2).saturating_add(1);
        }
        offset = offset.min(max);
        let old = last;
        last = hint - offset;
        offset = hint - old;
    } else {
        let max = n - hint;
        while offset < max {
            let matched = if right {
                !less(key(pivot), key(&values[(hint + offset) as usize]))?
            } else {
                less(key(&values[(hint + offset) as usize]), key(pivot))?
            };
            if !matched {
                break;
            }
            last = offset;
            offset = offset.saturating_mul(2).saturating_add(1);
        }
        offset = offset.min(max);
        last += hint;
        offset += hint;
    }
    last += 1;
    while last < offset {
        let middle = last + ((offset - last) >> 1);
        let lower = if right {
            !less(key(pivot), key(&values[middle as usize]))?
        } else {
            less(key(&values[middle as usize]), key(pivot))?
        };
        if lower {
            last = middle + 1;
        } else {
            offset = middle;
        }
    }
    Ok(offset as usize)
}

pub(super) fn adjacent<T: Clone>(
    values: &mut [T],
    start: usize,
    mut na: usize,
    mut nb: usize,
    min_gallop: &mut usize,
    key: &impl Fn(&T) -> &Key,
) -> Result<(), String> {
    let middle = start + na;
    let skip = gallop(&values[middle], &values[start..middle], 0, true, key)?;
    let start = start + skip;
    na -= skip;
    if na == 0 {
        return Ok(());
    }
    nb = gallop(&values[middle - 1], &values[middle..middle + nb], nb - 1, false, key)?;
    if nb == 0 {
        return Ok(());
    }
    if na <= nb {
        low(values, start, na, nb, min_gallop, key)
    } else {
        high(values, start, na, nb, min_gallop, key)
    }
}
fn low<T: Clone>(
    values: &mut [T],
    start: usize,
    mut na: usize,
    mut nb: usize,
    minimum: &mut usize,
    key: &impl Fn(&T) -> &Key,
) -> Result<(), String> {
    let middle = start + na;
    let temp = values[start..middle].to_vec();
    let (mut a, mut b, mut d) = (0, middle, start);
    values[d] = values[b].clone();
    d += 1;
    b += 1;
    nb -= 1;
    let result = (|| {
        if nb == 0 {
            return Ok(());
        }
        if na == 1 {
            for i in 0..nb {
                values[d + i] = values[b + i].clone();
            }
            values[d + nb] = temp[a].clone();
            na = 0;
            return Ok(());
        }
        let mut min = *minimum;
        loop {
            let (mut acount, mut bcount) = (0, 0);
            loop {
                if less(key(&values[b]), key(&temp[a]))? {
                    values[d] = values[b].clone();
                    d += 1;
                    b += 1;
                    bcount += 1;
                    acount = 0;
                    nb -= 1;
                    if nb == 0 {
                        return Ok(());
                    }
                    if bcount >= min {
                        break;
                    }
                } else {
                    values[d] = temp[a].clone();
                    d += 1;
                    a += 1;
                    acount += 1;
                    bcount = 0;
                    na -= 1;
                    if na == 1 {
                        for i in 0..nb {
                            values[d + i] = values[b + i].clone();
                        }
                        values[d + nb] = temp[a].clone();
                        na = 0;
                        return Ok(());
                    }
                    if acount >= min {
                        break;
                    }
                }
            }
            min += 1;
            loop {
                min -= usize::from(min > 1);
                *minimum = min;
                acount = gallop(&values[b], &temp[a..a + na], 0, true, key)?;
                values[d..d + acount].clone_from_slice(&temp[a..a + acount]);
                d += acount;
                a += acount;
                na -= acount;
                if na == 0 {
                    return Ok(());
                }
                if na == 1 {
                    for i in 0..nb {
                        values[d + i] = values[b + i].clone();
                    }
                    values[d + nb] = temp[a].clone();
                    na = 0;
                    return Ok(());
                }
                values[d] = values[b].clone();
                d += 1;
                b += 1;
                nb -= 1;
                if nb == 0 {
                    return Ok(());
                }
                bcount = gallop(&temp[a], &values[b..b + nb], 0, false, key)?;
                for i in 0..bcount {
                    values[d + i] = values[b + i].clone();
                }
                d += bcount;
                b += bcount;
                nb -= bcount;
                if nb == 0 {
                    return Ok(());
                }
                values[d] = temp[a].clone();
                d += 1;
                a += 1;
                na -= 1;
                if na == 1 {
                    for i in 0..nb {
                        values[d + i] = values[b + i].clone();
                    }
                    values[d + nb] = temp[a].clone();
                    na = 0;
                    return Ok(());
                }
                if acount < 7 && bcount < 7 {
                    break;
                }
            }
            min += 1;
            *minimum = min;
        }
    })();
    values[d..d + na].clone_from_slice(&temp[a..a + na]);
    result
}
fn high<T: Clone>(
    values: &mut [T],
    start: usize,
    mut na: usize,
    mut nb: usize,
    minimum: &mut usize,
    key: &impl Fn(&T) -> &Key,
) -> Result<(), String> {
    let middle = start + na;
    let temp = values[middle..middle + nb].to_vec();
    let (mut a, mut b, mut d) = ((middle - 1) as isize, (nb - 1) as isize, (middle + nb - 1) as isize);
    values[d as usize] = values[a as usize].clone();
    d -= 1;
    a -= 1;
    na -= 1;
    let result = (|| {
        if na == 0 {
            return Ok(());
        }
        if nb == 1 {
            for i in (0..na).rev() {
                values[(d - na as isize + 1 + i as isize) as usize] = values[start + i].clone();
            }
            values[(d - na as isize) as usize] = temp[b as usize].clone();
            nb = 0;
            return Ok(());
        }
        let mut min = *minimum;
        loop {
            let (mut acount, mut bcount) = (0, 0);
            loop {
                if less(key(&temp[b as usize]), key(&values[a as usize]))? {
                    values[d as usize] = values[a as usize].clone();
                    d -= 1;
                    a -= 1;
                    acount += 1;
                    bcount = 0;
                    na -= 1;
                    if na == 0 {
                        return Ok(());
                    }
                    if acount >= min {
                        break;
                    }
                } else {
                    values[d as usize] = temp[b as usize].clone();
                    d -= 1;
                    b -= 1;
                    bcount += 1;
                    acount = 0;
                    nb -= 1;
                    if nb == 1 {
                        for i in (0..na).rev() {
                            values[(d - na as isize + 1 + i as isize) as usize] = values[start + i].clone();
                        }
                        values[(d - na as isize) as usize] = temp[b as usize].clone();
                        nb = 0;
                        return Ok(());
                    }
                    if bcount >= min {
                        break;
                    }
                }
            }
            min += 1;
            loop {
                min -= usize::from(min > 1);
                *minimum = min;
                acount = na - gallop(&temp[b as usize], &values[start..start + na], na - 1, true, key)?;
                for _ in 0..acount {
                    values[d as usize] = values[a as usize].clone();
                    d -= 1;
                    a -= 1;
                }
                na -= acount;
                if na == 0 {
                    return Ok(());
                }
                values[d as usize] = temp[b as usize].clone();
                d -= 1;
                b -= 1;
                nb -= 1;
                if nb == 1 {
                    for i in (0..na).rev() {
                        values[(d - na as isize + 1 + i as isize) as usize] = values[start + i].clone();
                    }
                    values[(d - na as isize) as usize] = temp[b as usize].clone();
                    nb = 0;
                    return Ok(());
                }
                bcount = nb - gallop(&values[a as usize], &temp[..nb], nb - 1, false, key)?;
                for _ in 0..bcount {
                    values[d as usize] = temp[b as usize].clone();
                    d -= 1;
                    b -= 1;
                }
                nb -= bcount;
                if nb == 0 {
                    return Ok(());
                }
                if nb == 1 {
                    for i in (0..na).rev() {
                        values[(d - na as isize + 1 + i as isize) as usize] = values[start + i].clone();
                    }
                    values[(d - na as isize) as usize] = temp[b as usize].clone();
                    nb = 0;
                    return Ok(());
                }
                values[d as usize] = values[a as usize].clone();
                d -= 1;
                a -= 1;
                na -= 1;
                if na == 0 {
                    return Ok(());
                }
                if acount < 7 && bcount < 7 {
                    break;
                }
            }
            min += 1;
            *minimum = min;
        }
    })();
    for i in 0..nb {
        values[(d - nb as isize + 1 + i as isize) as usize] = temp[i].clone();
    }
    result
}

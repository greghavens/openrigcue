//! `punycode` and `idna` text encoders for writing a game file in its declared encoding: the text is
//! encoded in one streaming call that is never finalized.
//!
//! `idna` is a buffered encoder: without finalization the text after the last dot is kept back and
//! never written. Each label goes through ToASCII (nameprep with Unicode 3.2
//! NFKC, then Punycode). The tables (`builtin/idna_tables.bin`) hold
//! stringprep B.1, B.2, the prohibited tables C.1.2, C.2.2, C.3–C.9, D.1, D.2, the full NFKD of each
//! character under Unicode 3.2, and the combining classes and canonical compositions used for
//! composition.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

const DATA: &[u8] = include_bytes!("builtin/idna_tables.bin");

struct Tables {
    b1: Vec<(u32, u32)>,
    b2: HashMap<u32, Vec<char>>,
    prohibited: Vec<(u32, u32)>,
    d1: Vec<(u32, u32)>,
    d2: Vec<(u32, u32)>,
    nfkd: HashMap<u32, Vec<char>>,
    combining: Vec<(u32, u32, u8)>,
    compositions: HashMap<(u32, u32), u32>,
    first: HashSet<u32>,
}

struct Reader {
    pos: usize,
}

impl Reader {
    fn varint(&mut self) -> u32 {
        let mut value = 0u32;
        let mut shift = 0;
        loop {
            let byte = DATA[self.pos];
            self.pos += 1;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    fn set(&mut self) -> Vec<(u32, u32)> {
        let mut end = 0;
        (0..self.varint())
            .map(|_| {
                let start = end + self.varint();
                end = start + self.varint();
                (start, end)
            })
            .collect()
    }

    fn map(&mut self) -> HashMap<u32, Vec<char>> {
        let mut code = 0;
        (0..self.varint())
            .map(|_| {
                code += self.varint();
                let value = (0..self.varint()).filter_map(|_| char::from_u32(self.varint())).collect();
                (code, value)
            })
            .collect()
    }
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut r = Reader { pos: 0 };
        let b1 = r.set();
        let b2 = r.map();
        let prohibited = r.set();
        let d1 = r.set();
        let d2 = r.set();
        let nfkd = r.map();
        let mut end = 0;
        let combining = (0..r.varint())
            .map(|_| {
                let start = end + r.varint();
                end = start + r.varint();
                (start, end, r.varint() as u8)
            })
            .collect();
        let compositions: HashMap<(u32, u32), u32> =
            (0..r.varint()).map(|_| ((r.varint(), r.varint()), r.varint())).collect();
        let first = compositions.keys().map(|(first, _)| *first).collect();
        Tables { b1, b2, prohibited, d1, d2, nfkd, combining, compositions, first }
    })
}

fn in_set(set: &[(u32, u32)], c: char) -> bool {
    let code = u32::from(c);
    let index = set.partition_point(|(start, _)| *start <= code);
    index > 0 && code < set[index - 1].1
}

fn combining(c: u32) -> u8 {
    let set = &tables().combining;
    let index = set.partition_point(|(start, _, _)| *start <= c);
    if index > 0 && c < set[index - 1].1 {
        set[index - 1].2
    } else {
        0
    }
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;

/// Unicode 3.2 NFKC of `text`: decomposition with insertion-sort canonical
/// ordering, then composition.
fn nfkc(text: &[char]) -> Vec<char> {
    let tables = tables();
    let mut data: Vec<u32> = Vec::with_capacity(text.len());
    for c in text {
        match tables.nfkd.get(&u32::from(*c)) {
            Some(decomposed) => data.extend(decomposed.iter().map(|c| u32::from(*c))),
            None => data.push(u32::from(*c)),
        }
    }
    for i in 1..data.len() {
        let mut j = i;
        while j > 0 {
            let (previous, current) = (combining(data[j - 1]), combining(data[j]));
            if previous == 0 || current == 0 || previous <= current {
                break;
            }
            data.swap(j - 1, j);
            j -= 1;
        }
    }
    let mut skipped = HashSet::new();
    let mut out: Vec<u32> = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if skipped.contains(&i) {
            i += 1;
            continue;
        }
        let mut code = data[i];
        if (L_BASE..L_BASE + L_COUNT).contains(&code)
            && data.get(i + 1).is_some_and(|v| (V_BASE..V_BASE + V_COUNT).contains(v))
        {
            code = S_BASE + ((code - L_BASE) * V_COUNT + (data[i + 1] - V_BASE)) * T_COUNT;
            i += 2;
            if let Some(t) = data.get(i).filter(|t| T_BASE < **t && **t < T_BASE + T_COUNT) {
                code += t - T_BASE;
                i += 1;
            }
            out.push(code);
            continue;
        }
        out.push(code);
        if !tables.first.contains(&code) {
            i += 1;
            continue;
        }
        let mut i1 = i + 1;
        let mut blocked_class = 0;
        while i1 < data.len() {
            let code1 = data[i1];
            let class1 = combining(code1);
            if blocked_class != 0 {
                if class1 == 0 {
                    break;
                }
                if blocked_class >= class1 {
                    i1 += 1;
                    continue;
                }
            }
            let last = *out.last().unwrap_or(&0);
            match tables.compositions.get(&(last, code1)) {
                Some(composed) => {
                    *out.last_mut().unwrap_or(&mut 0) = *composed;
                    skipped.insert(i1);
                    i1 += 1;
                    if !tables.first.contains(composed) {
                        break;
                    }
                }
                None => {
                    if class1 == 0 {
                        break;
                    }
                    blocked_class = class1;
                    i1 += 1;
                }
            }
        }
        i += 1;
    }
    out.into_iter().filter_map(char::from_u32).collect()
}

/// Nameprep of a label; `None` when the label is not allowed.
fn nameprep(label: &[char]) -> Option<Vec<char>> {
    let tables = tables();
    let mut mapped = Vec::with_capacity(label.len());
    for c in label {
        if in_set(&tables.b1, *c) {
            continue;
        }
        match tables.b2.get(&u32::from(*c)) {
            Some(value) => mapped.extend_from_slice(value),
            None => mapped.push(*c),
        }
    }
    let label = nfkc(&mapped);
    if label.iter().any(|c| in_set(&tables.prohibited, *c)) {
        return None;
    }
    let rand_al: Vec<bool> = label.iter().map(|c| in_set(&tables.d1, *c)).collect();
    if rand_al.iter().any(|x| *x) {
        if label.iter().any(|c| in_set(&tables.d2, *c)) {
            return None;
        }
        if !rand_al[0] || !rand_al[rand_al.len() - 1] {
            return None;
        }
    }
    Some(label)
}

/// Why nameprep refuses a label, with indexes into the mapped, normalized label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameprepError {
    /// A prohibited character (tables C.1.2, C.2.2, C.3–C.9) at this index.
    Prohibited(usize),
    /// `Violation of BIDI requirement 2` at the first L character.
    Bidi2(usize),
    /// `Violation of BIDI requirement 3`: the first (`true`) or last character is not RandAL.
    Bidi3(bool),
}

/// Nameprep (returning the normalized label alongside any refusal) of a label that may hold lone surrogates (a decoded Punycode label can). A
/// surrogate is neither mapped nor decomposed and never combines, so NFKC applies to the runs between them;
/// surrogates are prohibited (table C.5).
pub(crate) fn nameprep_code_points(label: &[u32]) -> Result<Vec<u32>, (Vec<u32>, NameprepError)> {
    let tables = tables();
    let is_surrogate = |c: u32| (0xD800..0xE000).contains(&c);
    let mut normalized = Vec::with_capacity(label.len());
    let mut run = Vec::new();
    let flush = |run: &mut Vec<char>, out: &mut Vec<u32>| {
        out.extend(nfkc(run).into_iter().map(u32::from));
        run.clear();
    };
    for &code in label {
        let Some(c) = char::from_u32(code) else {
            flush(&mut run, &mut normalized);
            normalized.push(code);
            continue;
        };
        if in_set(&tables.b1, c) {
            continue;
        }
        match tables.b2.get(&code) {
            Some(value) => run.extend_from_slice(value),
            None => run.push(c),
        }
    }
    flush(&mut run, &mut normalized);
    let class = |code: u32, set: &[(u32, u32)]| char::from_u32(code).is_some_and(|c| in_set(set, c));
    if let Some(index) = normalized.iter().position(|&c| is_surrogate(c) || class(c, &tables.prohibited)) {
        return Err((normalized, NameprepError::Prohibited(index)));
    }
    let rand_al: Vec<bool> = normalized.iter().map(|&c| class(c, &tables.d1)).collect();
    if rand_al.iter().any(|x| *x) {
        if let Some(index) = normalized.iter().position(|&c| class(c, &tables.d2)) {
            return Err((normalized, NameprepError::Bidi2(index)));
        }
        if !rand_al[0] {
            return Err((normalized, NameprepError::Bidi3(true)));
        }
        if !rand_al[rand_al.len() - 1] {
            return Err((normalized, NameprepError::Bidi3(false)));
        }
    }
    Ok(normalized)
}

/// ToASCII of a label; `None` when it fails.
fn to_ascii(label: &[char]) -> Option<Vec<u8>> {
    if label.iter().all(char::is_ascii) {
        return (!label.is_empty() && label.len() < 64).then(|| label.iter().map(|c| *c as u8).collect());
    }
    let label = nameprep(label)?;
    if label.iter().all(char::is_ascii) {
        return (!label.is_empty() && label.len() < 64).then(|| label.iter().map(|c| *c as u8).collect());
    }
    let lower: String = label.iter().take(4).collect::<String>().to_lowercase();
    if lower.starts_with("xn--") {
        return None;
    }
    let mut ascii = b"xn--".to_vec();
    ascii.extend(punycode(&label));
    (ascii.len() < 64).then_some(ascii)
}

/// Streaming `idna` encode of `text` without finalizing; `None` when it fails.
pub fn idna(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() {
        return Some(Vec::new());
    }
    let chars: Vec<char> = text.chars().collect();
    let mut labels: Vec<&[char]> = chars.split(|c| matches!(c, '.' | '\u{3002}' | '\u{ff0e}' | '\u{ff61}')).collect();
    // A text ending in a dot ends with an empty label; otherwise, without finalization, the last label is kept back.
    let ends_with_dot = labels.last().is_some_and(|label| label.is_empty());
    labels.pop();
    let trailing_dot = ends_with_dot || !labels.is_empty();
    let mut result = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            result.push(b'.');
        }
        result.extend(to_ascii(label)?);
    }
    if trailing_dot {
        result.push(b'.');
    }
    Some(result)
}

/// Punycode encoding of the whole text.
pub fn punycode(text: &[char]) -> Vec<u8> {
    let base: Vec<u8> = text.iter().filter(|c| c.is_ascii()).map(|c| *c as u8).collect();
    let mut extended: Vec<char> = text.iter().filter(|c| !c.is_ascii()).copied().collect();
    extended.sort_unstable();
    extended.dedup();
    let deltas = insertion_unsort(text, &extended);
    let encoded = generate_integers(base.len() as u64, &deltas);
    if base.is_empty() {
        return encoded;
    }
    let mut out = base;
    out.push(b'-');
    out.extend(encoded);
    out
}

/// The insertion deltas of the non-basic code points, in code point order.
fn insertion_unsort(text: &[char], extended: &[char]) -> Vec<u64> {
    let mut old_char = 0x80u64;
    let mut result = Vec::new();
    let mut old_index: i64 = -1;
    for c in extended {
        let char_code = u64::from(u32::from(*c));
        let current_len = text.iter().filter(|x| **x < *c).count() as u64;
        let mut delta = (current_len + 1) * (char_code - old_char);
        let mut index: i64 = -1;
        for x in text {
            if x == c {
                index += 1;
                delta = delta.wrapping_add_signed(index - old_index);
                result.push(delta - 1);
                old_index = index;
                delta = 0;
            } else if x < c {
                index += 1;
            }
        }
        old_char = char_code;
    }
    result
}

fn threshold(j: u64, bias: u64) -> u64 {
    (36 * (j + 1)).saturating_sub(bias).clamp(1, 26)
}

const DIGITS: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

fn generate_generalized_integer(mut n: u64, bias: u64, out: &mut Vec<u8>) {
    let mut j = 0;
    loop {
        let t = threshold(j, bias);
        if n < t {
            out.push(DIGITS[n as usize]);
            return;
        }
        out.push(DIGITS[(t + (n - t) % (36 - t)) as usize]);
        n = (n - t) / (36 - t);
        j += 1;
    }
}

fn adapt(mut delta: u64, first: bool, num_chars: u64) -> u64 {
    delta = if first { delta / 700 } else { delta / 2 };
    delta += delta / num_chars;
    let mut divisions = 0;
    while delta > 455 {
        delta /= 35;
        divisions += 36;
    }
    divisions + (36 * delta / (delta + 38))
}

fn generate_integers(base_len: u64, deltas: &[u64]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut bias = 72;
    for (points, delta) in deltas.iter().enumerate() {
        generate_generalized_integer(*delta, bias, &mut result);
        bias = adapt(*delta, points == 0, base_len + points as u64 + 1);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes written for a text in each codec.
    #[test]
    fn written_as_cpython_writes() {
        assert_eq!(idna("abc.d\u{e9}f.ghi").as_deref(), Some(&b"abc.xn--df-bja."[..]));
        assert_eq!(idna("abc.d\u{e9}f").as_deref(), Some(&b"abc."[..]));
        assert_eq!(idna("<?xml version=\"1.0\"?>\n<a/>").as_deref(), Some(&b"<?xml version=\"1."[..]));
        assert_eq!(idna("no dot").as_deref(), Some(&b""[..]));
        assert_eq!(idna("a..b."), None, "empty label");
        assert_eq!(idna(&format!("{}.", "x".repeat(64))), None, "label too long");
        assert_eq!(idna("Stra\u{df}e.").as_deref(), Some(&b"strasse."[..]));
        assert_eq!(idna("\u{5d0}a.").as_deref(), None, "BIDI requirement 2");
        assert_eq!(punycode(&"<a>\u{e9}</a>".chars().collect::<Vec<_>>()), b"<a></a>-dya");
        assert_eq!(punycode(&"b\u{fc}cher".chars().collect::<Vec<_>>()), b"bcher-kva");
        assert_eq!(nfkc(&['\u{1100}', '\u{1161}', '\u{11a8}']), ['\u{ac01}']);
        assert_eq!(nfkc(&['e', '\u{301}', '\u{fb01}']), ['\u{e9}', 'f', 'i']);
    }
}

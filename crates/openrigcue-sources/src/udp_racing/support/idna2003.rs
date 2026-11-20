//! IDNA encoding of a host name: IDNA 2003
//! (RFC 3490) with Nameprep (RFC 3491) over the Unicode 3.2.0 database and Punycode (RFC 3492).
//!
//! The pieces:
//!
//! - encoding: the ASCII fast path with its label checks; otherwise the split at the four dots of
//!   section 3.1, an optional trailing dot, `ToASCII` per label and error positions shifted by
//!   the labels before;
//! - `ToASCII` and Nameprep (map B.1/B.2, NFKC, the prohibited tables C.1.2, C.2.2 and C.3-C.9, the bidi rules;
//!   unassigned code points are allowed);
//! - the B.2 and B.3 mappings: the exception table, else the lowercase mapping
//!   of Unicode 15.1.0, around two NFKC passes;
//! - NFKC normalization for the 3.2.0 database: only
//!   characters assigned in 3.2.0 decompose (with their 3.2.0 mappings), while the canonical ordering and the
//!   composition use the current combining classes and composition pairs;
//! - Punycode encoding.
//!
//! This is not UTS #46 or IDNA 2008: no deviation characters, no newer normalization data.

#[path = "idna2003_tables.rs"]
mod tables;

use tables::{
    B3_EXCEPTIONS, BIDI_L_3_2, BIDI_R_AL_3_2, CC_3_2, COMBINING, COMPOSITION, CO_3_2, DECOMPOSITION,
    DECOMPOSITION_DATA, LOWER, ZS_3_2,
};

/// The IDNA-encoded host as text: the ASCII name the resolver receives, or the text of the
/// encoding error.
pub(super) fn encode_hostname(hostname: &str) -> Result<String, String> {
    let input: Vec<char> = hostname.chars().collect();
    encode(&input).map_err(|error| error.message(&input))
}

/// An encoding error: the failing range and the reason; the input is supplied when it is printed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EncodeError {
    start: usize,
    end: usize,
    reason: String,
}

impl EncodeError {
    fn new(start: usize, end: usize, reason: impl Into<String>) -> EncodeError {
        EncodeError { start, end, reason: reason.into() }
    }

    /// The error's text for `input`.
    fn message(&self, input: &[char]) -> String {
        if self.start < input.len() && self.end <= input.len() && self.end == self.start + 1 {
            let bad = u32::from(input[self.start]);
            let shown = if bad <= 0xff {
                format!("\\x{bad:02x}")
            } else if bad <= 0xffff {
                format!("\\u{bad:04x}")
            } else {
                format!("\\U{bad:08x}")
            };
            format!("'idna' codec can't encode character '{shown}' in position {}: {}", self.start, self.reason)
        } else {
            // `end - 1` as a signed value.
            let last = self.end as i64 - 1;
            format!("'idna' codec can't encode characters in position {}-{last}: {}", self.start, self.reason)
        }
    }
}

/// IDNA section 3.1: `[.。．｡]`.
fn is_dot(c: char) -> bool {
    matches!(c, '\u{2e}' | '\u{3002}' | '\u{ff0e}' | '\u{ff61}')
}

/// Encodes a whole host name.
fn encode(input: &[char]) -> Result<String, EncodeError> {
    if input.is_empty() {
        return Ok(String::new());
    }
    if input.iter().all(char::is_ascii) {
        // ASCII name: fast path.
        let labels: Vec<&[char]> = input.split(|c| *c == '.').collect();
        let offset = |index: usize| labels[..index].iter().map(|label| label.len()).sum::<usize>() + index;
        for (index, label) in labels[..labels.len() - 1].iter().enumerate() {
            if label.is_empty() {
                let offset = offset(index);
                return Err(EncodeError::new(offset, offset + 1, "label empty"));
            }
        }
        for (index, label) in labels.iter().enumerate() {
            if label.len() >= 64 {
                let offset = offset(index);
                return Err(EncodeError::new(offset, offset + label.len(), "label too long"));
            }
        }
        return Ok(input.iter().collect());
    }
    let mut labels: Vec<&[char]> = input.split(|c| is_dot(*c)).collect();
    let trailing_dot = labels.last().is_some_and(|label| label.is_empty());
    if trailing_dot {
        labels.pop();
    }
    let mut result = String::new();
    for (index, label) in labels.iter().enumerate() {
        if !result.is_empty() {
            // Join with U+002E.
            result.push('.');
        }
        match to_ascii(label) {
            Ok(ascii) => result.push_str(&ascii),
            Err(error) => {
                let offset = labels[..index].iter().map(|label| label.len()).sum::<usize>() + index;
                return Err(EncodeError::new(offset + error.start, offset + error.end, error.reason));
            }
        }
    }
    if trailing_dot {
        result.push('.');
    }
    Ok(result)
}

/// The length checks both ASCII steps of `ToASCII` share.
fn checked_ascii(label: &[char]) -> Result<String, EncodeError> {
    if !label.is_empty() && label.len() < 64 {
        Ok(label.iter().collect())
    } else if label.is_empty() {
        Err(EncodeError::new(0, 1, "label empty"))
    } else {
        Err(EncodeError::new(0, label.len(), "label too long"))
    }
}

/// `ToASCII(label)`.
fn to_ascii(label: &[char]) -> Result<String, EncodeError> {
    // Step 1: try ASCII (UseSTD3ASCIIRules is false: skip to step 8).
    if label.iter().all(char::is_ascii) {
        return checked_ascii(label);
    }
    // Step 2: nameprep.
    let label = nameprep(label)?;
    // Step 4: try ASCII.
    if label.iter().all(char::is_ascii) {
        return checked_ascii(&label);
    }
    // Step 5: check the ACE prefix (the lower-cased label must not start with `xn--`).
    if label.len() >= 4 && label[..4].iter().map(char::to_ascii_lowercase).eq("xn--".chars()) {
        return Err(EncodeError::new(0, 4, "Label starts with ACE prefix"));
    }
    // Steps 6 and 7: Punycode behind the ACE prefix.
    let ascii = format!("xn--{}", punycode_encode(&label));
    // Step 8: check the size.
    if ascii.len() < 64 {
        Ok(ascii)
    } else {
        Err(EncodeError::new(0, label.len(), "label too long"))
    }
}

/// Nameprep of a label (query strings: AllowUnassigned is true).
fn nameprep(label: &[char]) -> Result<Vec<char>, EncodeError> {
    // Map.
    let mut mapped = Vec::with_capacity(label.len());
    for c in label {
        if in_table_b1(*c) {
            // Map to nothing.
            continue;
        }
        mapped.extend(map_table_b2(*c));
    }
    // Normalize.
    let label = nfkc(&mapped);
    // Prohibit.
    for (index, c) in label.iter().enumerate() {
        if prohibited(*c) {
            return Err(EncodeError::new(index, index + 1, format!("Invalid character {}", python_repr(*c))));
        }
    }
    // Check bidi.
    let r_and_al: Vec<bool> = label.iter().map(|c| in_ranges(BIDI_R_AL_3_2, *c)).collect();
    if r_and_al.iter().any(|found| *found) {
        // 2) A string with a RandALCat character must not contain an LCat character.
        for (index, c) in label.iter().enumerate() {
            if in_ranges(BIDI_L_3_2, *c) {
                return Err(EncodeError::new(index, index + 1, "Violation of BIDI requirement 2"));
            }
        }
        // 3) A RandALCat character must be the first and the last character.
        if !r_and_al[0] {
            return Err(EncodeError::new(0, 1, "Violation of BIDI requirement 3"));
        }
        if !r_and_al[r_and_al.len() - 1] {
            return Err(EncodeError::new(label.len() - 1, label.len(), "Violation of BIDI requirement 3"));
        }
    }
    Ok(label)
}

fn in_ranges(table: &[(u32, u32)], c: char) -> bool {
    let code = u32::from(c);
    table
        .binary_search_by(|(first, last)| {
            if code < *first {
                std::cmp::Ordering::Greater
            } else if code > *last {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Whether the character is in table B.1.
fn in_table_b1(c: char) -> bool {
    matches!(
        u32::from(c),
        0xAD | 0x34F | 0x1806 | 0x180B | 0x180C | 0x180D | 0x200B | 0x200C | 0x200D | 0x2060 | 0xFEFF | 0xFE00..=0xFE0F
    )
}

/// The tables Nameprep prohibits: C.1.2, C.2.2, C.3, C.4, C.6, C.7, C.8 and C.9 (C.5, the surrogates, cannot
/// occur in this text).
fn prohibited(c: char) -> bool {
    let code = u32::from(c);
    // Table C.1.2: a space separator other than U+0020.
    let c12 = in_ranges(ZS_3_2, c) && c != ' ';
    // Table C.2.2.
    let c22 = code >= 128
        && (in_ranges(CC_3_2, c)
            || matches!(
                code,
                0x6DD
                    | 0x70F
                    | 0x180E
                    | 0x200C
                    | 0x200D
                    | 0x2028
                    | 0x2029
                    | 0xFEFF
                    | 0x2060..=0x2063
                    | 0x206A..=0x206F
                    | 0xFFF9..=0xFFFC
                    | 0x1D173..=0x1D17A
            ));
    let c3 = in_ranges(CO_3_2, c);
    // Table C.4.
    let c4 = code >= 0xFDD0 && (code < 0xFDF0 || matches!(code & 0xFFFF, 0xFFFE | 0xFFFF));
    let c6 = (0xFFF9..=0xFFFD).contains(&code);
    let c7 = (0x2FF0..=0x2FFB).contains(&code);
    let c8 = matches!(code, 0x340 | 0x341 | 0x200E | 0x200F | 0x202A..=0x202E | 0x206A..=0x206F);
    let c9 = matches!(code, 0xE0001 | 0xE0020..=0xE007F);
    c12 || c22 || c3 || c4 || c6 || c7 || c8 || c9
}

/// The quoted form of a prohibited character: the printable ones (U+0340, U+0341, the
/// ideographic description characters, U+FFFC and U+FFFD) quoted as they are, the others escaped.
fn python_repr(c: char) -> String {
    let code = u32::from(c);
    if matches!(code, 0x340 | 0x341 | 0x2FF0..=0x2FFB | 0xFFFC | 0xFFFD) {
        format!("'{c}'")
    } else if code <= 0xff {
        format!("'\\x{code:02x}'")
    } else if code <= 0xffff {
        format!("'\\u{code:04x}'")
    } else {
        format!("'\\U{code:08x}'")
    }
}

/// The table B.3 mapping: the exception table, else the lowercase mapping.
fn map_table_b3(c: char, out: &mut Vec<char>) {
    let code = u32::from(c);
    if let Ok(index) = B3_EXCEPTIONS.binary_search_by_key(&code, |(key, _)| *key) {
        out.extend(B3_EXCEPTIONS[index].1.chars());
    } else if code == 0x130 {
        // The one unconditional full lowercase mapping of SpecialCasing: U+0130 lowers to `i` U+0307.
        out.extend(['i', '\u{307}']);
    } else if let Ok(index) = LOWER.binary_search_by_key(&code, |(key, _)| *key) {
        out.push(char::from_u32(LOWER[index].1).unwrap_or(c));
    } else {
        out.push(c);
    }
}

/// The table B.2 mapping.
fn map_table_b2(a: char) -> Vec<char> {
    let mut al = Vec::new();
    map_table_b3(a, &mut al);
    let b = nfkc(&al);
    let mut bl = Vec::new();
    for c in &b {
        map_table_b3(*c, &mut bl);
    }
    let c = nfkc(&bl);
    if b != c {
        c
    } else {
        al
    }
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The canonical combining class of a code point.
fn combining(code: u32) -> u8 {
    COMBINING
        .binary_search_by(|(first, last, _)| {
            if code < *first {
                std::cmp::Ordering::Greater
            } else if code > *last {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .map_or(0, |index| COMBINING[index].2)
}

/// Compatibility decomposition (NFKD) for the 3.2.0 database.
fn nfkd(input: &[char]) -> Vec<u32> {
    let mut output: Vec<u32> = Vec::with_capacity(input.len());
    let mut stack: Vec<u32> = Vec::new();
    for c in input {
        stack.push(u32::from(*c));
        while let Some(code) = stack.pop() {
            // Hangul decomposition adds up to three characters in a single step.
            if (S_BASE..S_BASE + S_COUNT).contains(&code) {
                let s_index = code - S_BASE;
                output.push(L_BASE + s_index / N_COUNT);
                output.push(V_BASE + (s_index % N_COUNT) / T_COUNT);
                let t = T_BASE + s_index % T_COUNT;
                if t != T_BASE {
                    output.push(t);
                }
                continue;
            }
            // Other decompositions: none for a character unassigned in 3.2.0.
            match DECOMPOSITION.binary_search_by_key(&code, |(key, _, _, _)| *key) {
                Ok(index) => {
                    let (_, _, offset, length) = DECOMPOSITION[index];
                    let data = &DECOMPOSITION_DATA[usize::from(offset)..usize::from(offset) + usize::from(length)];
                    // Copy the decomposition onto the stack, in reverse order.
                    stack.extend(data.iter().rev());
                }
                Err(_) => output.push(code),
            }
        }
    }
    // Sort canonically.
    let mut index = 1;
    while index < output.len() {
        let current = combining(output[index]);
        let mut position = index;
        if current != 0 {
            while position > 0 {
                let previous = combining(output[position - 1]);
                if previous == 0 || previous <= current {
                    break;
                }
                output.swap(position, position - 1);
                position -= 1;
            }
        }
        index += 1;
    }
    output
}

/// Whether `code` is the first character of a composition pair.
fn composes_first(code: u32) -> bool {
    let index = COMPOSITION.partition_point(|(first, _, _)| *first < code);
    COMPOSITION.get(index).is_some_and(|(first, _, _)| *first == code)
}

/// The composite of `first` and `second`, if the composition table has one.
fn compose(first: u32, second: u32) -> Option<u32> {
    COMPOSITION.binary_search_by_key(&(first, second), |(a, b, _)| (*a, *b)).ok().map(|index| COMPOSITION[index].2)
}

/// NFKC normalization for the 3.2.0 database. No quick check is
/// made.
fn nfkc(input: &[char]) -> Vec<char> {
    let input = nfkd(input);
    let len = input.len();
    let mut output: Vec<u32> = Vec::with_capacity(len);
    let mut skipped: Vec<usize> = Vec::new();
    let mut i = 0;
    'characters: while i < len {
        for index in 0..skipped.len() {
            if skipped[index] == i {
                // This character is skipped. Remove it from the list.
                skipped.swap_remove(index);
                i += 1;
                continue 'characters;
            }
        }
        // Hangul composition. <LV, T> pairs need no check: the data is always decomposed.
        let mut code = input[i];
        if (L_BASE..L_BASE + L_COUNT).contains(&code)
            && i + 1 < len
            && (V_BASE..V_BASE + V_COUNT).contains(&input[i + 1])
        {
            let l_index = code - L_BASE;
            let v_index = input[i + 1] - V_BASE;
            code = S_BASE + (l_index * V_COUNT + v_index) * T_COUNT;
            i += 2;
            if i < len && T_BASE < input[i] && input[i] < T_BASE + T_COUNT {
                code += input[i] - T_BASE;
                i += 1;
            }
            output.push(code);
            continue;
        }
        if !composes_first(code) {
            output.push(code);
            i += 1;
            continue;
        }
        // Find the next unblocked character; the base is output now and updated when characters combine.
        let mut base = code;
        let mut i1 = i + 1;
        let mut comb = 0u8;
        while i1 < len {
            let code1 = input[i1];
            let comb1 = combining(code1);
            if comb != 0 {
                if comb1 == 0 {
                    break;
                }
                if comb >= comb1 {
                    // The character is blocked.
                    i1 += 1;
                    continue;
                }
            }
            match compose(base, code1) {
                None => {
                    // `i1` cannot be combined with the base. A starter ends the search; otherwise its
                    // combining class is recorded.
                    if comb1 == 0 {
                        break;
                    }
                    comb = comb1;
                    i1 += 1;
                }
                Some(composite) => {
                    // Replace the original character and mark the second one unused.
                    base = composite;
                    skipped.push(i1);
                    i1 += 1;
                    if !composes_first(base) {
                        break;
                    }
                }
            }
        }
        output.push(base);
        i += 1;
    }
    output.into_iter().filter_map(char::from_u32).collect()
}

/// Punycode encoding (RFC 3492).
fn punycode_encode(text: &[char]) -> String {
    // 3.1 Basic code point segregation.
    let base: String = text.iter().filter(|c| c.is_ascii()).collect();
    let mut extended: Vec<char> = text.iter().copied().filter(|c| !c.is_ascii()).collect();
    extended.sort_unstable();
    extended.dedup();
    let deltas = insertion_unsort(text, &extended);
    let integers = generate_integers(base.chars().count() as u64, &deltas);
    if base.is_empty() {
        integers
    } else {
        format!("{base}-{integers}")
    }
}

/// 3.2 Insertion unsort coding.
fn insertion_unsort(text: &[char], extended: &[char]) -> Vec<u64> {
    let mut oldchar: u64 = 0x80;
    let mut result = Vec::new();
    let mut oldindex: i64 = -1;
    for c in extended {
        let mut index: i64 = -1;
        let mut pos: i64 = -1;
        let char_code = u64::from(u32::from(*c));
        // The number of characters below `char`.
        let curlen = text.iter().filter(|other| u64::from(u32::from(**other)) < char_code).count() as u64;
        let mut delta: i64 = ((curlen + 1) * (char_code - oldchar)) as i64;
        loop {
            // The next occurrence of `c`.
            let mut found = false;
            loop {
                pos += 1;
                if pos as usize == text.len() {
                    break;
                }
                let other = text[pos as usize];
                if other == *c {
                    index += 1;
                    found = true;
                    break;
                } else if other < *c {
                    index += 1;
                }
            }
            if !found {
                break;
            }
            delta += index - oldindex;
            result.push((delta - 1) as u64);
            oldindex = index;
            delta = 0;
        }
        oldchar = char_code;
    }
    result
}

/// Punycode parameters: tmin = 1, tmax = 26, base = 36.
fn threshold(j: u64, bias: u64) -> u64 {
    let res = 36 * (j as i64 + 1) - bias as i64;
    res.clamp(1, 26) as u64
}

const DIGITS: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// 3.3 Generalized variable-length integers.
fn generate_generalized_integer(mut n: u64, bias: u64, result: &mut String) {
    let mut j = 0;
    loop {
        let t = threshold(j, bias);
        if n < t {
            result.push(char::from(DIGITS[n as usize]));
            return;
        }
        result.push(char::from(DIGITS[(t + (n - t) % (36 - t)) as usize]));
        n = (n - t) / (36 - t);
        j += 1;
    }
}

/// 3.4 Bias adaptation.
fn adapt(mut delta: u64, first: bool, numchars: u64) -> u64 {
    if first {
        delta /= 700;
    } else {
        delta /= 2;
    }
    delta += delta / numchars;
    // (base - tmin) * tmax / 2 = 455
    let mut divisions = 0;
    while delta > 455 {
        delta /= 35;
        divisions += 36;
    }
    divisions + (36 * delta / (delta + 38))
}

/// Punycode parameters: initial bias = 72, damp = 700, skew = 38.
fn generate_integers(baselen: u64, deltas: &[u64]) -> String {
    let mut result = String::new();
    let mut bias = 72;
    for (points, delta) in deltas.iter().enumerate() {
        generate_generalized_integer(*delta, bias, &mut result);
        bias = adapt(*delta, points == 0, baselen + points as u64 + 1);
    }
    result
}

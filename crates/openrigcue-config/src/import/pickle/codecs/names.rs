//! Unicode character name lookup for `\N{...}` escapes (Unicode 15.1):
//! Hangul syllables and the derived ideograph names, then the packed name DAWG, with name aliases
//! resolved and named sequences refused. Names match case-insensitively (ASCII).
//!
//! `data/unicode_names.bin` holds the packed name DAWG, the position-to-code-point table and the
//! name aliases, each as a little-endian `u32` count and
//! then the bytes / `u32` values.

use std::sync::OnceLock;

const DATA: &[u8] = include_bytes!("data/unicode_names.bin");

const ALIASES_START: u32 = 0xF0000;
const ALIASES_END: u32 = 0xF01D9;
const NAMED_SEQUENCES_START: u32 = 0xF0200;
const NAMED_SEQUENCES_END: u32 = 0xF03CD;

const DERIVED_PREFIXES: [&str; 3] = ["HANGUL SYLLABLE ", "CJK UNIFIED IDEOGRAPH-", "TANGUT IDEOGRAPH-"];
const DERIVED_RANGES: [(u32, u32, usize); 13] = [
    (0x3400, 0x4DBF, 1),
    (0x4E00, 0x9FFF, 1),
    (0xAC00, 0xD7A3, 0),
    (0x17000, 0x187F7, 2),
    (0x18D00, 0x18D08, 2),
    (0x20000, 0x2A6DF, 1),
    (0x2A700, 0x2B739, 1),
    (0x2B740, 0x2B81D, 1),
    (0x2B820, 0x2CEA1, 1),
    (0x2CEB0, 0x2EBE0, 1),
    (0x2EBF0, 0x2EE5D, 1),
    (0x30000, 0x3134A, 1),
    (0x31350, 0x323AF, 1),
];

const HANGUL: [[&str; 3]; 28] = [
    ["G", "A", ""],
    ["GG", "AE", "G"],
    ["N", "YA", "GG"],
    ["D", "YAE", "GS"],
    ["DD", "EO", "N"],
    ["R", "E", "NJ"],
    ["M", "YEO", "NH"],
    ["B", "YE", "D"],
    ["BB", "O", "L"],
    ["S", "WA", "LG"],
    ["SS", "WAE", "LM"],
    ["", "OE", "LB"],
    ["J", "YO", "LS"],
    ["JJ", "U", "LT"],
    ["C", "WEO", "LP"],
    ["K", "WE", "LH"],
    ["T", "WI", "M"],
    ["P", "YU", "B"],
    ["H", "EU", "BS"],
    ["", "YI", "S"],
    ["", "I", "SS"],
    ["", "", "NG"],
    ["", "", "J"],
    ["", "", "C"],
    ["", "", "K"],
    ["", "", "T"],
    ["", "", "P"],
    ["", "", "H"],
];
const L_COUNT: usize = 19;
const V_COUNT: usize = 21;
const T_COUNT: usize = 28;

struct Tables {
    dawg: &'static [u8],
    positions: Vec<u32>,
    aliases: Vec<u32>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        assert_eq!(&DATA[..8], b"SRSUCN1\0");
        let mut at = 8;
        let mut count = || {
            let value = u32::from_le_bytes(DATA[at..at + 4].try_into().expect("count")) as usize;
            at += 4;
            value
        };
        let dawg_len = count();
        let dawg = &DATA[at..at + dawg_len];
        at += dawg_len;
        let words = |at: &mut usize| {
            let len = u32::from_le_bytes(DATA[*at..*at + 4].try_into().expect("count")) as usize;
            *at += 4;
            let values =
                DATA[*at..*at + 4 * len].as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w)).collect::<Vec<_>>();
            *at += 4 * len;
            values
        };
        let positions = words(&mut at);
        let aliases = words(&mut at);
        Tables { dawg, positions, aliases }
    })
}

/// The code point `\N{name}` gives, `None` for an unknown name.
pub(super) fn lookup(name: &[u8]) -> Option<u32> {
    let code = getcode(name)?;
    if (NAMED_SEQUENCES_START..NAMED_SEQUENCES_END).contains(&code) {
        return None;
    }
    if (ALIASES_START..ALIASES_END).contains(&code) {
        return tables().aliases.get((code - ALIASES_START) as usize).copied();
    }
    Some(code)
}

fn starts_with_ignore_case(name: &[u8], prefix: &str) -> bool {
    name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// The longest entry of the column that prefixes `text`, with the first of equal length.
fn find_syllable(text: &[u8], column: usize, count: usize) -> (usize, Option<usize>) {
    let mut len: isize = -1;
    let mut pos = None;
    for (index, row) in HANGUL.iter().take(count).enumerate() {
        let syllable = row[column];
        if syllable.len() as isize <= len {
            continue;
        }
        if starts_with_ignore_case(text, syllable) {
            len = syllable.len() as isize;
            pos = Some(index);
        }
    }
    (len.max(0) as usize, pos)
}

fn parse_hex(digits: &[u8]) -> Option<u32> {
    if !(4..=6).contains(&digits.len()) || digits[0] == b'0' {
        return None;
    }
    let mut value = 0u32;
    for digit in digits {
        value = value * 16 + char::from(*digit).to_digit(16)?;
    }
    (value <= 0x10FFFF).then_some(value)
}

/// The code point of a name: algorithmic names first, then the name table.
fn getcode(name: &[u8]) -> Option<u32> {
    let prefix = DERIVED_PREFIXES.iter().position(|prefix| starts_with_ignore_case(name, prefix));
    match prefix {
        Some(0) => {
            let mut pos = 16;
            let (len, l) = find_syllable(&name[pos..], 0, L_COUNT);
            pos += len;
            let (len, v) = find_syllable(&name[pos..], 1, V_COUNT);
            pos += len;
            let (len, t) = find_syllable(&name[pos..], 2, T_COUNT);
            pos += len;
            let (l, v, t) = (l?, v?, t?);
            (pos == name.len()).then(|| 0xAC00 + ((l * V_COUNT + v) * T_COUNT + t) as u32)
        }
        Some(index) => {
            let code = parse_hex(&name[DERIVED_PREFIXES[index].len()..])?;
            let id =
                DERIVED_RANGES.iter().find(|(first, last, _)| (*first..=*last).contains(&code)).map(|range| range.2);
            (id == Some(index)).then_some(code)
        }
        None => {
            let position = dawg_lookup(name)?;
            tables().positions.get(position).copied()
        }
    }
}

fn varint(dawg: &[u8], mut index: usize) -> (usize, u32) {
    let mut result = 0u32;
    let mut shift = 0;
    loop {
        let byte = dawg[index];
        result |= u32::from(byte & 0x7F) << shift;
        index += 1;
        shift += 7;
        if byte & 0x80 == 0 {
            return (index, result);
        }
    }
}

/// Walks the packed DAWG: the index of the name among all names.
fn dawg_lookup(name: &[u8]) -> Option<usize> {
    let dawg = tables().dawg;
    let descendants = |node: usize| varint(dawg, node).1 >> 1;
    let mut string_pos = 0;
    let mut node = 0usize;
    let mut result = 0usize;
    while string_pos < name.len() {
        let (mut edge, number) = varint(dawg, node);
        let is_final = number & 1 == 1;
        let mut previous_target = edge;
        let mut first = true;
        loop {
            let (next, number) = varint(dawg, edge);
            if number == 0 && first {
                return None;
            }
            first = false;
            let last_edge = number & 1 == 1;
            let rest = number >> 1;
            let len_is_one = rest & 1 == 1;
            let target = previous_target + (rest >> 1) as usize;
            previous_target = target;
            let (size, label) = if len_is_one { (1, next) } else { (usize::from(dawg[next]), next + 1) };
            // Match one edge label.
            let matched = if size > 1 && string_pos + size > name.len() {
                Some(false)
            } else {
                let mut verdict = Some(true);
                for i in 0..size {
                    if dawg[label + i] != name[string_pos + i].to_ascii_uppercase() {
                        verdict = if i > 0 { None } else { Some(false) };
                        break;
                    }
                }
                verdict
            };
            match matched {
                None => return None,
                Some(true) => {
                    if is_final {
                        result += 1;
                    }
                    string_pos += size;
                    node = target;
                    break;
                }
                Some(false) => {
                    if last_edge {
                        return None;
                    }
                    result += descendants(target) as usize;
                    edge = label + size;
                }
            }
        }
    }
    (varint(dawg, node).1 & 1 == 1).then_some(result)
}

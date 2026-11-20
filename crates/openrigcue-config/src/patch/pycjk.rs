//! Multi-byte (CJK) text encoders for writing a game file in its declared encoding: the text is encoded
//! in one streaming call that is never finalized.
//!
//! The tables (`builtin/cjk_codecs.bin`) hold, for every code point, the bytes
//! the codec encodes it to (for ISO-2022 and HZ split into the designation and the code), the characters an
//! encoder holds back when they end the input (a JIS X 0213 or HKSCS character that could start a pair) and the
//! pairs that encode as one code.
//!
//! Without finalization, a held-back character at the end of the text is never written, and the ISO-2022 / HZ encoders
//! do not return to ASCII at the end.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

const DATA: &[u8] = include_bytes!("builtin/cjk_codecs.bin");
/// Code length marking a GB 18030 four-byte code stored as its linear index.
const GB18030_LINEAR: u8 = 15;
const ESC_ASCII: &[u8] = b"\x1b(B";
const SO: u8 = 0x0e;
const SI: u8 = 0x0f;

#[derive(Debug, Clone, Copy)]
struct Code {
    /// 0 for a codec without designations, else the index of the designation plus one.
    designation: u8,
    len: u8,
    value: u64,
}

impl Code {
    fn write(self, out: &mut Vec<u8>) {
        if self.len == GB18030_LINEAR {
            let mut linear = self.value;
            let b4 = 0x30 + (linear % 10) as u8;
            linear /= 10;
            let b3 = 0x81 + (linear % 126) as u8;
            linear /= 126;
            let b2 = 0x30 + (linear % 10) as u8;
            linear /= 10;
            out.extend([0x81 + linear as u8, b2, b3, b4]);
        } else {
            let bytes = self.value.to_be_bytes();
            out.extend_from_slice(&bytes[8 - usize::from(self.len)..]);
        }
    }
}

#[derive(Debug)]
struct Run {
    start: u32,
    count: u32,
    designation: u8,
    len: u8,
    value: u64,
}

#[derive(Debug)]
struct Codec {
    name: String,
    designations: Vec<Vec<u8>>,
    runs: Vec<Run>,
    pairs: HashMap<(u32, u32), Code>,
    pending: HashSet<u32>,
}

impl Codec {
    fn single(&self, c: u32) -> Option<Code> {
        let index = self.runs.partition_point(|run| run.start <= c).checked_sub(1)?;
        let run = &self.runs[index];
        (c < run.start + run.count).then(|| Code {
            designation: run.designation,
            len: run.len,
            value: run.value + u64::from(c - run.start),
        })
    }

    /// The code for the character at `index` and how many characters it takes; `Ok(None)` when it is held back
    /// at the end; `Err` when it cannot be encoded.
    fn next(&self, chars: &[u32], index: usize) -> Result<Option<(Code, usize)>, ()> {
        let c = chars[index];
        if self.pending.contains(&c) {
            let Some(&next) = chars.get(index + 1) else { return Ok(None) };
            if let Some(code) = self.pairs.get(&(c, next)) {
                return Ok(Some((*code, 2)));
            }
        }
        self.single(c).map(|code| Some((code, 1))).ok_or(())
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn varint(&mut self) -> u128 {
        let mut value = 0u128;
        let mut shift = 0;
        loop {
            let byte = self.data[self.pos];
            self.pos += 1;
            value |= u128::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    fn u32(&mut self) -> u32 {
        u32::try_from(self.varint()).unwrap_or(u32::MAX)
    }

    fn byte(&mut self) -> u8 {
        self.pos += 1;
        self.data[self.pos - 1]
    }

    fn bytes(&mut self) -> Vec<u8> {
        let len = self.varint() as usize;
        self.pos += len;
        self.data[self.pos - len..self.pos].to_vec()
    }
}

fn codecs() -> &'static [Codec] {
    static CODECS: OnceLock<Vec<Codec>> = OnceLock::new();
    CODECS.get_or_init(|| {
        let mut reader = Reader { data: DATA, pos: 0 };
        let count = reader.varint();
        (0..count)
            .map(|_| {
                let name = String::from_utf8(reader.bytes()).unwrap_or_default();
                let designations = (0..reader.varint()).map(|_| reader.bytes()).collect();
                let mut runs = Vec::new();
                let (mut cp_end, mut value_end) = (0u32, 0i128);
                for _ in 0..reader.varint() {
                    let start = cp_end + reader.u32();
                    let count = reader.u32() + 1;
                    let packed = reader.byte();
                    let zigzag = reader.varint();
                    let delta = (zigzag >> 1) as i128 ^ -((zigzag & 1) as i128);
                    let value = value_end + delta;
                    runs.push(Run { start, count, designation: packed >> 4, len: packed & 0x0f, value: value as u64 });
                    cp_end = start + count;
                    value_end = value + i128::from(count);
                }
                let pairs = (0..reader.varint())
                    .map(|_| {
                        let (first, second) = (reader.u32(), reader.u32());
                        let packed = reader.byte();
                        let code = Code { designation: packed >> 4, len: packed & 0x0f, value: reader.varint() as u64 };
                        ((first, second), code)
                    })
                    .collect();
                let pending = (0..reader.varint()).map(|_| reader.u32()).collect();
                Codec { name, designations, runs, pairs, pending }
            })
            .collect()
    })
}

fn codec(module: &str) -> Option<&'static Codec> {
    codecs().iter().find(|codec| codec.name == module)
}

/// Whether `module` is one of these codecs.
pub fn is_multibyte(module: &str) -> bool {
    codec(module).is_some()
}

/// Encodes `text` without finalizing; `None` when a character cannot be encoded, and
/// for a module that is not one of these codecs.
pub fn encode(module: &str, text: &str) -> Option<Vec<u8>> {
    let codec = codec(module)?;
    let chars: Vec<u32> = text.chars().map(u32::from).collect();
    match module {
        "hz" => encode_hz(codec, &chars),
        _ if !codec.designations.is_empty() => encode_iso2022(codec, &chars),
        _ => encode_stateless(codec, &chars),
    }
}

fn encode_stateless(codec: &Codec, chars: &[u32]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let Some((code, taken)) = codec.next(chars, index).ok()? else { break };
        code.write(&mut out);
        index += taken;
    }
    Some(out)
}

/// ISO-2022 encoding: ASCII returns G0 to ASCII (`ESC ( B`) and shifts in; other characters take the first
/// designation that has them, designated to G0 (shifting in first) or to G1 and shifted out (ISO-2022-KR).
fn encode_iso2022(codec: &Codec, chars: &[u32]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut g0, mut g1, mut shifted) = (0u8, 0u8, false);
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if c < 0x80 {
            if g0 != 0 {
                out.extend_from_slice(ESC_ASCII);
                g0 = 0;
            }
            if shifted {
                out.push(SI);
                shifted = false;
            }
            out.push(c as u8);
            index += 1;
            continue;
        }
        let Some((code, taken)) = codec.next(chars, index).ok()? else { break };
        let escape = codec.designations.get(usize::from(code.designation).checked_sub(1)?)?;
        if escape.starts_with(b"\x1b$)") {
            if g1 != code.designation {
                out.extend_from_slice(escape);
                g1 = code.designation;
            }
            if !shifted {
                out.push(SO);
                shifted = true;
            }
        } else {
            if shifted {
                out.push(SI);
                shifted = false;
            }
            if g0 != code.designation {
                out.extend_from_slice(escape);
                g0 = code.designation;
            }
        }
        code.write(&mut out);
        index += taken;
    }
    Some(out)
}

/// HZ encoding: `~` doubled, GB 2312 text between `~{` and `~}`.
fn encode_hz(codec: &Codec, chars: &[u32]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut gb = false;
    for &c in chars {
        if c < 0x80 {
            if gb {
                out.extend_from_slice(b"~}");
                gb = false;
            }
            out.push(c as u8);
            if c == u32::from(b'~') {
                out.push(b'~');
            }
            continue;
        }
        let code = codec.single(c)?;
        if !gb {
            out.extend_from_slice(b"~{");
            gb = true;
        }
        code.write(&mut out);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes written for a text in each codec.
    #[test]
    fn written_as_cpython_writes() {
        let cases: &[(&str, &str, &[u8])] = &[
            ("iso2022_jp", "<a>日本</a>日本", b"<a>\x1b$BF|K\\\x1b(B</a>\x1b$BF|K\\"),
            ("iso2022_kr", "abc한국", b"abc\x1b$)C\x0eGQ19"),
            ("iso2022_kr", "한\n한", b"\x1b$)C\x0eGQ\x0f\n\x0eGQ"),
            ("iso2022_jp_2", "αé", b"\x1b$B&A\x1b$(D+1"),
            ("iso2022_jp_2", "\u{a5}a", b"\x1b(J\\\x1b(Ba"),
            ("iso2022_jp_ext", "\u{ff71}a", b"\x1b(I1\x1b(Ba"),
            ("iso2022_jp_2004", "\u{304b}\u{309a}", b"\x1b$(Q$w"),
            ("iso2022_jp_2004", "\u{304b}", b""),
            ("iso2022_jp_2004", "\u{304b}x", b"\x1b$B$+\x1b(Bx"),
            ("hz", "abc中文", b"abc~{VPND"),
            ("hz", "~é\né", b"~~~{(&~}\n~{(&"),
            ("big5hkscs", "\u{ca}\u{304}x\u{ca}", b"\x88bx"),
            ("shift_jis_2004", "\u{304b}\u{309a}x", b"\x82\xf5x"),
            ("shift_jis", "<a>\u{3042}\u{a5}</a>", b"<a>\x82\xa0\\</a>"),
            ("gb18030", "\u{80}\u{20ac}\u{10000}", b"\x81\x30\x81\x30\xa2\xe3\x90\x30\x81\x30"),
            ("euc_kr", "\u{ac02}", b"\xa4\xd4\xa4\xa1\xa4\xbf\xa4\xa2"),
            ("euc_jis_2004", "\u{4e02}", b"\x8f\xa1\xa2"),
        ];
        for (codec, text, expected) in cases {
            assert_eq!(encode(codec, text).as_deref(), Some(*expected), "{codec} {text:?}");
        }
        assert_eq!(encode("shift_jis", "\u{e9}"), None);
        assert_eq!(encode("iso2022_jp", "\u{e9}"), None);
        assert_eq!(encode("hz", "\u{10000}"), None);
        assert_eq!(encode("utf_8", "a"), None);
        assert!(is_multibyte("johab") && !is_multibyte("cp1252"));
    }
}

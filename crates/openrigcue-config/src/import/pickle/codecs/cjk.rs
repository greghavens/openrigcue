//! Multi-byte (CJK) text decoders: one stateful decoder run over the whole input, with the error
//! handler applied at each error, for the Japanese, Chinese (simplified and traditional), Korean,
//! Hong Kong and ISO-2022 encodings.
//!
//! `data/cjk_decode.bin` holds the decode maps (Japanese, simplified Chinese, Korean, traditional Chinese,
//! Hong Kong, and the JIS X 0213 pairs): per map a `u32` value count,
//! 256 `(u32 offset or u32::MAX, u8 bottom, u8 top)` rows and the `u16` values (`u32` for the JIS X 0213 pair
//! map); then the GB 18030 BMP ranges (count, `first, last, base` triples) and the three Big5-HKSCS plane hints.

use super::{error, Errors, NativeException};
use std::sync::OnceLock;

const DATA: &[u8] = include_bytes!("data/cjk_decode.bin");
const UNIINV: u32 = 0xFFFE;
const EMPBASE: u32 = 0x20000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Map {
    Jisx0208,
    Jisx0212,
    Cp932ext,
    Jisx0213_1Bmp,
    Jisx0213_2Bmp,
    Jisx0213_1Emp,
    Jisx0213_2Emp,
    Jisx0213Pair,
    Gb2312,
    Gbkext,
    Gb18030ext,
    Ksx1001,
    Cp949ext,
    Big5,
    Cp950ext,
    Big5hkscs,
}

struct DecodeMap {
    rows: Vec<(u32, u8, u8)>,
    values: Vec<u32>,
}

struct Tables {
    maps: Vec<DecodeMap>,
    gb18030_ranges: Vec<(u32, u32, u32)>,
    hints: [Vec<u8>; 3],
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        assert_eq!(&DATA[..8], b"SRSCJK1\0");
        let mut at = 8;
        let u32_at = |at: &mut usize| {
            let value = u32::from_le_bytes(DATA[*at..*at + 4].try_into().expect("u32"));
            *at += 4;
            value
        };
        let mut maps = Vec::new();
        for index in 0..16 {
            let wide = index == Map::Jisx0213Pair as usize;
            let count = u32_at(&mut at) as usize;
            let mut rows = Vec::with_capacity(256);
            for _ in 0..256 {
                let offset = u32_at(&mut at);
                rows.push((offset, DATA[at], DATA[at + 1]));
                at += 2;
            }
            let values = (0..count)
                .map(|_| {
                    if wide {
                        u32_at(&mut at)
                    } else {
                        let value = u16::from_le_bytes([DATA[at], DATA[at + 1]]);
                        at += 2;
                        u32::from(value)
                    }
                })
                .collect();
            maps.push(DecodeMap { rows, values });
        }
        let ranges = u32_at(&mut at) as usize;
        let gb18030_ranges = (0..ranges).map(|_| (u32_at(&mut at), u32_at(&mut at), u32_at(&mut at))).collect();
        let hint = |at: &mut usize| {
            let len = u32_at(at) as usize;
            let bytes = DATA[*at..*at + len].to_vec();
            *at += len;
            bytes
        };
        let hints = [hint(&mut at), hint(&mut at), hint(&mut at)];
        assert_eq!(at, DATA.len());
        Tables { maps, gb18030_ranges, hints }
    })
}

/// Looks `(c1, c2)` up in a decode map.
fn trymap(map: Map, c1: u8, c2: u8) -> Option<u32> {
    let table = &tables().maps[map as usize];
    let (offset, bottom, top) = table.rows[usize::from(c1)];
    if offset == u32::MAX || c2 < bottom || c2 > top {
        return None;
    }
    let value = table.values[offset as usize + usize::from(c2 - bottom)];
    (value != UNIINV).then_some(value)
}

/// A multi-byte codec, identified by its encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Codec {
    ShiftJis,
    Cp932,
    EucJp,
    ShiftJis2004,
    EucJis2004,
    EucJisx0213,
    ShiftJisx0213,
    Gb2312,
    Gbk,
    Gb18030,
    Hz,
    EucKr,
    Cp949,
    Johab,
    Big5,
    Cp950,
    Big5hkscs,
    Iso2022(Variant),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Variant {
    Kr,
    Jp,
    Jp1,
    Jp2,
    Jp2004,
    Jp3,
    JpExt,
}

impl Codec {
    /// The codec a codec module name stands for.
    pub(super) fn named(module: &str) -> Option<Codec> {
        Some(match module {
            "shift_jis" => Codec::ShiftJis,
            "cp932" => Codec::Cp932,
            "euc_jp" => Codec::EucJp,
            "shift_jis_2004" => Codec::ShiftJis2004,
            "euc_jis_2004" => Codec::EucJis2004,
            "euc_jisx0213" => Codec::EucJisx0213,
            "shift_jisx0213" => Codec::ShiftJisx0213,
            "gb2312" => Codec::Gb2312,
            "gbk" => Codec::Gbk,
            "gb18030" => Codec::Gb18030,
            "hz" => Codec::Hz,
            "euc_kr" => Codec::EucKr,
            "cp949" => Codec::Cp949,
            "johab" => Codec::Johab,
            "big5" => Codec::Big5,
            "cp950" => Codec::Cp950,
            "big5hkscs" => Codec::Big5hkscs,
            "iso2022_kr" => Codec::Iso2022(Variant::Kr),
            "iso2022_jp" => Codec::Iso2022(Variant::Jp),
            "iso2022_jp_1" => Codec::Iso2022(Variant::Jp1),
            "iso2022_jp_2" => Codec::Iso2022(Variant::Jp2),
            "iso2022_jp_2004" => Codec::Iso2022(Variant::Jp2004),
            "iso2022_jp_3" => Codec::Iso2022(Variant::Jp3),
            "iso2022_jp_ext" => Codec::Iso2022(Variant::JpExt),
            _ => return None,
        })
    }

    fn encoding(self) -> &'static str {
        match self {
            Codec::ShiftJis => "shift_jis",
            Codec::Cp932 => "cp932",
            Codec::EucJp => "euc_jp",
            Codec::ShiftJis2004 => "shift_jis_2004",
            Codec::EucJis2004 => "euc_jis_2004",
            Codec::EucJisx0213 => "euc_jisx0213",
            Codec::ShiftJisx0213 => "shift_jisx0213",
            Codec::Gb2312 => "gb2312",
            Codec::Gbk => "gbk",
            Codec::Gb18030 => "gb18030",
            Codec::Hz => "hz",
            Codec::EucKr => "euc_kr",
            Codec::Cp949 => "cp949",
            Codec::Johab => "johab",
            Codec::Big5 => "big5",
            Codec::Cp950 => "cp950",
            Codec::Big5hkscs => "big5hkscs",
            Codec::Iso2022(variant) => match variant {
                Variant::Kr => "iso2022_kr",
                Variant::Jp => "iso2022_jp",
                Variant::Jp1 => "iso2022_jp_1",
                Variant::Jp2 => "iso2022_jp_2",
                Variant::Jp2004 => "iso2022_jp_2004",
                Variant::Jp3 => "iso2022_jp_3",
                Variant::JpExt => "iso2022_jp_ext",
            },
        }
    }
}

/// Why a decoder stopped: an illegal sequence of that many bytes (`> 0`), too few bytes, or an internal error.
enum Stop {
    Illegal(usize),
    TooFew,
    Internal,
}

/// Decoder state: bytes 0–3 are the ISO-2022 designations and byte 4 its flags; HZ uses byte 0.
struct State([u8; 8]);

/// Decodes `data` with this codec and the given error handler.
pub(super) fn decode(codec: Codec, data: &[u8], errors: Errors) -> Result<Vec<u32>, NativeException> {
    let mut state = State([0; 8]);
    if let Codec::Iso2022(_) = codec {
        state.0[4] = 0;
        state.0[0] = CHARSET_ASCII;
        state.0[1] = CHARSET_ASCII;
        state.0[2] = CHARSET_ASCII;
    }
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    while pos < data.len() {
        let stop = match run(codec, &mut state, data, &mut pos, &mut out) {
            Ok(()) => break,
            Err(stop) => stop,
        };
        let (size, reason) = match stop {
            Stop::Illegal(size) => (size, "illegal multibyte sequence"),
            Stop::TooFew => (data.len() - pos, "incomplete multibyte sequence"),
            Stop::Internal => return Err(error("RuntimeError", "internal codec error")),
        };
        let (replacement, position) = errors.handle(codec.encoding(), data, pos, pos + size, reason)?;
        out.extend(replacement);
        pos = position;
    }
    Ok(out)
}

fn run(codec: Codec, state: &mut State, data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    match codec {
        Codec::ShiftJis => shift_jis(data, pos, out),
        Codec::Cp932 => cp932(data, pos, out),
        Codec::EucJp => euc_jp(data, pos, out),
        Codec::ShiftJis2004 => shift_jis_2004(data, pos, out, false),
        Codec::ShiftJisx0213 => shift_jis_2004(data, pos, out, true),
        Codec::EucJis2004 => euc_jis_2004(data, pos, out, false),
        Codec::EucJisx0213 => euc_jis_2004(data, pos, out, true),
        Codec::Gb2312 => gb2312(data, pos, out),
        Codec::Gbk => gbk(data, pos, out),
        Codec::Gb18030 => gb18030(data, pos, out),
        Codec::Hz => hz(state, data, pos, out),
        Codec::EucKr => euc_kr(data, pos, out),
        Codec::Cp949 => cp949(data, pos, out),
        Codec::Johab => johab(data, pos, out),
        Codec::Big5 => big5(data, pos, out),
        Codec::Cp950 => cp950(data, pos, out),
        Codec::Big5hkscs => big5hkscs(data, pos, out),
        Codec::Iso2022(variant) => iso2022(variant, state, data, pos, out),
    }
}

fn require(data: &[u8], pos: usize, n: usize) -> Result<(), Stop> {
    if data.len() - pos < n {
        Err(Stop::TooFew)
    } else {
        Ok(())
    }
}

/// The JIS X 0208 row/cell of a Shift_JIS lead and trail byte (`c2` already validated).
fn sjis_to_jis(c: u8, c2: u8) -> (u8, u8) {
    let c1 = if c < 0xE0 { c.wrapping_sub(0x81) } else { c.wrapping_sub(0xC1) };
    let c2 = if c2 < 0x80 { c2 - 0x40 } else { c2 - 0x41 };
    let row = (2u8.wrapping_mul(c1)).wrapping_add(if c2 < 0x5E { 0 } else { 1 });
    let cell = (if c2 < 0x5E { c2 } else { c2 - 0x5E }) + 0x21;
    (row, cell)
}

fn sjis_trail_invalid(c2: u8) -> bool {
    c2 < 0x40 || (c2 > 0x7E && c2 < 0x80) || c2 > 0xFC
}

fn cp932(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c <= 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        } else if (0xA0..=0xDF).contains(&c) {
            out.push(if c == 0xA0 { 0xF8F0 } else { 0xFEC0 + u32::from(c) });
            *pos += 1;
            continue;
        } else if c >= 0xFD {
            out.push(0xF8F1 - 0xFD + u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        let c2 = data[*pos + 1];
        if let Some(decoded) = trymap(Map::Cp932ext, c, c2) {
            out.push(decoded);
        } else if (0x81..=0x9F).contains(&c) || (0xE0..=0xEA).contains(&c) {
            if sjis_trail_invalid(c2) {
                return Err(Stop::Illegal(1));
            }
            let (row, cell) = sjis_to_jis(c, c2);
            out.push(trymap(Map::Jisx0208, row.wrapping_add(0x21), cell).ok_or(Stop::Illegal(1))?);
        } else if (0xF0..=0xF9).contains(&c) {
            if (0x40..=0x7E).contains(&c2) || (0x80..=0xFC).contains(&c2) {
                let cell = u32::from(if c2 < 0x80 { c2 - 0x40 } else { c2 - 0x41 });
                out.push(0xE000 + 188 * u32::from(c - 0xF0) + cell);
            } else {
                return Err(Stop::Illegal(1));
            }
        } else {
            return Err(Stop::Illegal(1));
        }
        *pos += 2;
    }
    Ok(())
}

fn shift_jis(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
        } else if (0xA1..=0xDF).contains(&c) {
            out.push(0xFEC0 + u32::from(c));
        } else if (0x81..=0x9F).contains(&c) || (0xE0..=0xEA).contains(&c) {
            require(data, *pos, 2)?;
            let c2 = data[*pos + 1];
            if sjis_trail_invalid(c2) {
                return Err(Stop::Illegal(1));
            }
            let (row, cell) = sjis_to_jis(c, c2);
            let row = row.wrapping_add(0x21);
            if row == 0x21 && cell == 0x40 {
                out.push(0xFF3C);
            } else {
                out.push(trymap(Map::Jisx0208, row, cell).ok_or(Stop::Illegal(1))?);
            }
            *pos += 2;
            continue;
        } else {
            return Err(Stop::Illegal(1));
        }
        *pos += 1;
    }
    Ok(())
}

fn euc_jp(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        if c == 0x8E {
            require(data, *pos, 2)?;
            let c2 = data[*pos + 1];
            if !(0xA1..=0xDF).contains(&c2) {
                return Err(Stop::Illegal(1));
            }
            out.push(0xFEC0 + u32::from(c2));
            *pos += 2;
        } else if c == 0x8F {
            require(data, *pos, 3)?;
            let decoded =
                trymap(Map::Jisx0212, data[*pos + 1] ^ 0x80, data[*pos + 2] ^ 0x80).ok_or(Stop::Illegal(1))?;
            out.push(decoded);
            *pos += 3;
        } else {
            require(data, *pos, 2)?;
            let c2 = data[*pos + 1];
            if c == 0xA1 && c2 == 0xC0 {
                out.push(0xFF3C);
            } else {
                out.push(trymap(Map::Jisx0208, c ^ 0x80, c2 ^ 0x80).ok_or(Stop::Illegal(1))?);
            }
            *pos += 2;
        }
    }
    Ok(())
}

/// The plane-1 codes JIS X 0213:2000 lacks (they were added in 2004).
fn jisx0213_2000_invalid(c1: u8, c2: u8) -> bool {
    matches!((c1, c2), (0x2E, 0x21) | (0x2F, 0x7E) | (0x4F, 0x54) | (0x4F, 0x7E) | (0x74, 0x27) | (0x7E, 0x7A..=0x7E))
}

/// JIS X 0213 plane 1 (`jisx0208`, `1_bmp`, `1_emp`, the pairs), as `euc_jis_2004` / `shift_jis_2004` try it.
fn jisx0213_plane1(c1: u8, c2: u8, out: &mut Vec<u32>) -> bool {
    if let Some(decoded) = trymap(Map::Jisx0208, c1, c2).or_else(|| trymap(Map::Jisx0213_1Bmp, c1, c2)) {
        out.push(decoded);
    } else if let Some(code) = trymap(Map::Jisx0213_1Emp, c1, c2) {
        out.push(EMPBASE | code);
    } else if let Some(code) = trymap(Map::Jisx0213Pair, c1, c2) {
        out.push(code >> 16);
        out.push(code & 0xFFFF);
    } else {
        return false;
    }
    true
}

fn jisx0213_plane2(c1: u8, c2: u8, out: &mut Vec<u32>) -> bool {
    if let Some(decoded) = trymap(Map::Jisx0213_2Bmp, c1, c2) {
        out.push(decoded);
    } else if let Some(code) = trymap(Map::Jisx0213_2Emp, c1, c2) {
        out.push(EMPBASE | code);
    } else {
        return false;
    }
    true
}

fn euc_jis_2004(data: &[u8], pos: &mut usize, out: &mut Vec<u32>, jis2000: bool) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        if c == 0x8E {
            require(data, *pos, 2)?;
            let c2 = data[*pos + 1];
            if !(0xA1..=0xDF).contains(&c2) {
                return Err(Stop::Illegal(1));
            }
            out.push(0xFEC0 + u32::from(c2));
            *pos += 2;
        } else if c == 0x8F {
            require(data, *pos, 3)?;
            let (c2, c3) = (data[*pos + 1] ^ 0x80, data[*pos + 2] ^ 0x80);
            if jis2000 && c2 == 0x7D && c3 == 0x3B {
                out.push(0x9B1D);
            } else if !jisx0213_plane2(c2, c3, out) {
                out.push(trymap(Map::Jisx0212, c2, c3).ok_or(Stop::Illegal(1))?);
            }
            *pos += 3;
        } else {
            require(data, *pos, 2)?;
            let (c1, c2) = (c ^ 0x80, data[*pos + 1] ^ 0x80);
            if jis2000 && jisx0213_2000_invalid(c1, c2) {
                return Err(Stop::Illegal(2));
            } else if c1 == 0x21 && c2 == 0x40 {
                out.push(0xFF3C);
            } else if c1 == 0x22 && c2 == 0x32 {
                out.push(0xFF5E);
            } else if !jisx0213_plane1(c1, c2, out) {
                return Err(Stop::Illegal(1));
            }
            *pos += 2;
        }
    }
    Ok(())
}

fn shift_jis_2004(data: &[u8], pos: &mut usize, out: &mut Vec<u32>, jis2000: bool) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        match c {
            0..=0x5B | 0x5D..=0x7D | 0x7F => out.push(u32::from(c)),
            0x5C => out.push(0xA5),
            0x7E => out.push(0x203E),
            0xA1..=0xDF => out.push(0xFEC0 + u32::from(c)),
            0x81..=0x9F | 0xE0..=0xFC => {
                require(data, *pos, 2)?;
                let c2 = data[*pos + 1];
                if sjis_trail_invalid(c2) {
                    return Err(Stop::Illegal(1));
                }
                let (mut c1, c2) = sjis_to_jis(c, c2);
                if c1 < 0x5E {
                    c1 += 0x21;
                    if jis2000 && jisx0213_2000_invalid(c1, c2) {
                        return Err(Stop::Illegal(2));
                    }
                    if !jisx0213_plane1(c1, c2, out) {
                        return Err(Stop::Illegal(1));
                    }
                } else {
                    c1 = if c1 >= 0x67 {
                        c1.wrapping_add(0x07)
                    } else if c1 >= 0x63 || c1 == 0x5F {
                        c1 - 0x37
                    } else {
                        c1 - 0x3D
                    };
                    if jis2000 && c1 == 0x7D && c2 == 0x3B {
                        out.push(0x9B1D);
                    } else if !jisx0213_plane2(c1, c2, out) {
                        return Err(Stop::Illegal(1));
                    }
                }
                *pos += 2;
                continue;
            }
            _ => return Err(Stop::Illegal(1)),
        }
        *pos += 1;
    }
    Ok(())
}

fn gb2312(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        out.push(trymap(Map::Gb2312, c ^ 0x80, data[*pos + 1] ^ 0x80).ok_or(Stop::Illegal(1))?);
        *pos += 2;
    }
    Ok(())
}

/// Decodes one GBK two-byte sequence.
fn gbk_decode(c: u8, c2: u8) -> Option<u32> {
    match (c, c2) {
        (0xA1, 0xAA) => Some(0x2014),
        (0xA8, 0x44) => Some(0x2015),
        (0xA1, 0xA4) => Some(0x00B7),
        _ => trymap(Map::Gb2312, c ^ 0x80, c2 ^ 0x80).or_else(|| trymap(Map::Gbkext, c, c2)),
    }
}

fn gbk(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        out.push(gbk_decode(c, data[*pos + 1]).ok_or(Stop::Illegal(1))?);
        *pos += 2;
    }
    Ok(())
}

fn gb18030(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        let c2 = data[*pos + 1];
        if (0x30..=0x39).contains(&c2) {
            require(data, *pos, 4)?;
            let (c3, c4) = (data[*pos + 2], data[*pos + 3]);
            if !(0x81..=0xFE).contains(&c) || !(0x81..=0xFE).contains(&c3) || !(0x30..=0x39).contains(&c4) {
                return Err(Stop::Illegal(1));
            }
            let (c, c2, c3, c4) =
                (u32::from(c - 0x81), u32::from(c2 - 0x30), u32::from(c3 - 0x81), u32::from(c4 - 0x30));
            if c < 4 {
                let lseq = (c * 10 + c2) * 1260 + c3 * 10 + c4;
                if lseq < 39420 {
                    let ranges = &tables().gb18030_ranges;
                    let mut index = 0;
                    while lseq >= ranges[index + 1].2 {
                        index += 1;
                    }
                    let (first, _, base) = ranges[index];
                    out.push(first - base + lseq);
                    *pos += 4;
                    continue;
                }
            } else if c >= 15 {
                let lseq = 0x10000 + ((c - 15) * 10 + c2) * 1260 + c3 * 10 + c4;
                if lseq <= 0x10FFFF {
                    out.push(lseq);
                    *pos += 4;
                    continue;
                }
            }
            return Err(Stop::Illegal(1));
        }
        out.push(gbk_decode(c, c2).or_else(|| trymap(Map::Gb18030ext, c, c2)).ok_or(Stop::Illegal(1))?);
        *pos += 2;
    }
    Ok(())
}

fn hz(state: &mut State, data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c == b'~' {
            require(data, *pos, 2)?;
            let gb = state.0[0];
            match (data[*pos + 1], gb) {
                (b'~', 0) => out.push(u32::from(b'~')),
                (b'{', 0) => state.0[0] = 1,
                (b'\n', 0) => {}
                (b'}', 1) => state.0[0] = 0,
                _ => return Err(Stop::Illegal(1)),
            }
            *pos += 2;
            continue;
        }
        if c & 0x80 != 0 {
            return Err(Stop::Illegal(1));
        }
        if state.0[0] == 0 {
            out.push(u32::from(c));
            *pos += 1;
        } else {
            require(data, *pos, 2)?;
            out.push(trymap(Map::Gb2312, c, data[*pos + 1]).ok_or(Stop::Illegal(1))?);
            *pos += 2;
        }
    }
    Ok(())
}

const KR_NONE: u16 = 127;
const CGK2U_CHOSEONG: [u16; 30] = [
    0, 1, KR_NONE, 2, KR_NONE, KR_NONE, 3, 4, 5, KR_NONE, KR_NONE, KR_NONE, KR_NONE, KR_NONE, KR_NONE, KR_NONE, 6, 7,
    8, KR_NONE, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
];
const CGK2U_JONGSEONG: [u16; 30] = [
    1, 2, 3, 4, 5, 6, 7, KR_NONE, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, KR_NONE, 18, 19, 20, 21, 22, KR_NONE, 23, 24,
    25, 26, 27,
];

fn euc_kr(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        if c == 0xA4 && data[*pos + 1] == 0xD4 {
            // KS X 1001:1998 make-up sequence.
            require(data, *pos, 8)?;
            let b = &data[*pos..*pos + 8];
            if b[2] != 0xA4 || b[4] != 0xA4 || b[6] != 0xA4 {
                return Err(Stop::Illegal(1));
            }
            let cho = if (0xA1..=0xBE).contains(&b[3]) { CGK2U_CHOSEONG[usize::from(b[3] - 0xA1)] } else { KR_NONE };
            let jung = if (0xBF..=0xD3).contains(&b[5]) { u16::from(b[5] - 0xBF) } else { KR_NONE };
            let jong = if b[7] == 0xD4 {
                0
            } else if (0xA1..=0xBE).contains(&b[7]) {
                CGK2U_JONGSEONG[usize::from(b[7] - 0xA1)]
            } else {
                KR_NONE
            };
            if cho == KR_NONE || jung == KR_NONE || jong == KR_NONE {
                return Err(Stop::Illegal(1));
            }
            out.push(0xAC00 + u32::from(cho) * 588 + u32::from(jung) * 28 + u32::from(jong));
            *pos += 8;
        } else {
            out.push(trymap(Map::Ksx1001, c ^ 0x80, data[*pos + 1] ^ 0x80).ok_or(Stop::Illegal(1))?);
            *pos += 2;
        }
    }
    Ok(())
}

fn cp949(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        let c2 = data[*pos + 1];
        out.push(
            trymap(Map::Ksx1001, c ^ 0x80, c2 ^ 0x80)
                .or_else(|| trymap(Map::Cp949ext, c, c2))
                .ok_or(Stop::Illegal(1))?,
        );
        *pos += 2;
    }
    Ok(())
}

const FILL: u8 = 0xFD;
const NONE: u8 = 0xFF;
const JOHABIDX_CHOSEONG: [u8; 32] = [
    NONE, FILL, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
    0x11, 0x12, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE,
];
const JOHABIDX_JUNGSEONG: [u8; 32] = [
    NONE, NONE, FILL, 0x00, 0x01, 0x02, 0x03, 0x04, NONE, NONE, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, NONE, NONE, 0x0b,
    0x0c, 0x0d, 0x0e, 0x0f, 0x10, NONE, NONE, 0x11, 0x12, 0x13, 0x14, NONE, NONE,
];
const JOHABIDX_JONGSEONG: [u8; 32] = [
    NONE, FILL, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, NONE,
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, NONE, NONE,
];
const JOHABJAMO_CHOSEONG: [u8; 32] = [
    NONE, FILL, 0x31, 0x32, 0x34, 0x37, 0x38, 0x39, 0x41, 0x42, 0x43, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c,
    0x4d, 0x4e, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE,
];
const JOHABJAMO_JUNGSEONG: [u8; 32] = [
    NONE, NONE, FILL, 0x4f, 0x50, 0x51, 0x52, 0x53, NONE, NONE, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, NONE, NONE, 0x5a,
    0x5b, 0x5c, 0x5d, 0x5e, 0x5f, NONE, NONE, 0x60, 0x61, 0x62, 0x63, NONE, NONE,
];
const JOHABJAMO_JONGSEONG: [u8; 32] = [
    NONE, FILL, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, NONE,
    0x42, 0x44, 0x45, 0x46, 0x47, 0x48, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, NONE, NONE,
];

fn johab(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        let c2 = data[*pos + 1];
        if c < 0xD8 {
            let c_cho = usize::from((c >> 2) & 0x1F);
            let c_jung = usize::from(((c << 3) | (c2 >> 5)) & 0x1F);
            let c_jong = usize::from(c2 & 0x1F);
            let (i_cho, i_jung, i_jong) =
                (JOHABIDX_CHOSEONG[c_cho], JOHABIDX_JUNGSEONG[c_jung], JOHABIDX_JONGSEONG[c_jong]);
            if i_cho == NONE || i_jung == NONE || i_jong == NONE {
                return Err(Stop::Illegal(1));
            }
            let decoded = match (i_cho == FILL, i_jung == FILL, i_jong == FILL) {
                (true, true, true) => 0x3000,
                (true, true, false) => 0x3100 | u32::from(JOHABJAMO_JONGSEONG[c_jong]),
                (true, false, true) => 0x3100 | u32::from(JOHABJAMO_JUNGSEONG[c_jung]),
                (true, false, false) => return Err(Stop::Illegal(1)),
                (false, true, true) => 0x3100 | u32::from(JOHABJAMO_CHOSEONG[c_cho]),
                (false, true, false) => return Err(Stop::Illegal(1)),
                (false, false, jong_fill) => {
                    0xAC00
                        + u32::from(i_cho) * 588
                        + u32::from(i_jung) * 28
                        + if jong_fill { 0 } else { u32::from(i_jong) }
                }
            };
            out.push(decoded);
            *pos += 2;
        } else {
            if c == 0xDF
                || c > 0xF9
                || c2 < 0x31
                || (0x80..0x91).contains(&c2)
                || c2 & 0x7F == 0x7F
                || (c == 0xDA && (0xA1..=0xD3).contains(&c2))
            {
                return Err(Stop::Illegal(1));
            }
            let t1 = if c < 0xE0 { 2 * (i32::from(c) - 0xD9) } else { 2 * i32::from(c) - 0x197 } as u8;
            let t2 = if c2 < 0x91 { c2 - 0x31 } else { c2 - 0x43 };
            let t1 = t1.wrapping_add(if t2 < 0x5E { 0 } else { 1 }).wrapping_add(0x21);
            let t2 = (if t2 < 0x5E { t2 } else { t2 - 0x5E }) + 0x21;
            out.push(trymap(Map::Ksx1001, t1, t2).ok_or(Stop::Illegal(1))?);
            *pos += 2;
        }
    }
    Ok(())
}

fn big5(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        out.push(trymap(Map::Big5, c, data[*pos + 1]).ok_or(Stop::Illegal(1))?);
        *pos += 2;
    }
    Ok(())
}

fn cp950(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        let c2 = data[*pos + 1];
        out.push(trymap(Map::Cp950ext, c, c2).or_else(|| trymap(Map::Big5, c, c2)).ok_or(Stop::Illegal(1))?);
        *pos += 2;
    }
    Ok(())
}

/// The linear index of a Big5-HKSCS lead/trail byte pair.
fn bh2s(c1: u8, c2: u8) -> i32 {
    (i32::from(c1) - 0x87) * (0xFE - 0x40 + 1) + (i32::from(c2) - 0x40)
}

fn big5hkscs(data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    while *pos < data.len() {
        let c = data[*pos];
        if c < 0x80 {
            out.push(u32::from(c));
            *pos += 1;
            continue;
        }
        require(data, *pos, 2)?;
        let c2 = data[*pos + 1];
        if !(0xC6..=0xC8).contains(&c) || (c < 0xC7 && c2 < 0xA1) {
            if let Some(decoded) = trymap(Map::Big5, c, c2) {
                out.push(decoded);
                *pos += 2;
                continue;
            }
        }
        if let Some(decoded) = trymap(Map::Big5hkscs, c, c2) {
            let s = bh2s(c, c2);
            let hints = &tables().hints;
            let (hint, s) = if (bh2s(0x87, 0x40)..=bh2s(0xA0, 0xFE)).contains(&s) {
                (&hints[0], s - bh2s(0x87, 0x40))
            } else if (bh2s(0xC6, 0xA1)..=bh2s(0xC8, 0xFE)).contains(&s) {
                (&hints[1], s - bh2s(0xC6, 0xA1))
            } else if (bh2s(0xF9, 0xD6)..=bh2s(0xFE, 0xFE)).contains(&s) {
                (&hints[2], s - bh2s(0xF9, 0xD6))
            } else {
                return Err(Stop::Internal);
            };
            let s = s as usize;
            let plane2 = hint.get(s >> 3).is_some_and(|byte| byte & (1 << (s & 7)) != 0);
            out.push(if plane2 { decoded | 0x20000 } else { decoded });
            *pos += 2;
            continue;
        }
        let pair = match (u32::from(c) << 8) | u32::from(c2) {
            0x8862 => (0x00CA, 0x0304),
            0x8864 => (0x00CA, 0x030C),
            0x88A3 => (0x00EA, 0x0304),
            0x88A5 => (0x00EA, 0x030C),
            _ => return Err(Stop::Illegal(1)),
        };
        out.push(pair.0);
        out.push(pair.1);
        *pos += 2;
    }
    Ok(())
}

const ESC: u8 = 0x1B;
const SO: u8 = 0x0E;
const SI: u8 = 0x0F;
const LF: u8 = 0x0A;
const DBCS: u8 = 0x80;
const CHARSET_ASCII: u8 = b'B';
const CHARSET_ISO8859_1: u8 = b'A';
const CHARSET_ISO8859_7: u8 = b'F';
const F_SHIFTED: u8 = 0x01;
const F_ESCTHROUGHOUT: u8 = 0x02;
const NO_SHIFT: u8 = 0x01;
const USE_G2: u8 = 0x02;
const USE_JISX0208_EXT: u8 = 0x04;

/// The `decoder` of an ISO-2022 designation.
#[derive(Clone, Copy)]
enum Designation {
    Ksx1001,
    Jisx0208,
    Jisx0212,
    Jisx0213_2000_1,
    Jisx0213_2000_2,
    Jisx0213_2004_1,
    Jisx0213_2004_2,
    Gb2312,
    Jisx0201R,
    Jisx0201K,
    Dummy,
}

/// The configuration of an ISO-2022 variant: flags and `(mark, width, decoder)` designations in registry order.
fn config(variant: Variant) -> (u8, &'static [(u8, usize, Designation)]) {
    use Designation as D;
    const KSX1001: (u8, usize, Designation) = (b'C' | DBCS, 2, D::Ksx1001);
    const JISX0208: (u8, usize, Designation) = (b'B' | DBCS, 2, D::Jisx0208);
    const JISX0208_O: (u8, usize, Designation) = (b'@' | DBCS, 2, D::Jisx0208);
    const JISX0212: (u8, usize, Designation) = (b'D' | DBCS, 2, D::Jisx0212);
    const JISX0201_R: (u8, usize, Designation) = (b'J', 1, D::Jisx0201R);
    const JISX0201_K: (u8, usize, Designation) = (b'I', 1, D::Jisx0201K);
    const GB2312: (u8, usize, Designation) = (b'A' | DBCS, 2, D::Gb2312);
    const ISO8859_1: (u8, usize, Designation) = (CHARSET_ISO8859_1, 1, D::Dummy);
    const ISO8859_7: (u8, usize, Designation) = (CHARSET_ISO8859_7, 1, D::Dummy);
    const J2000_1: (u8, usize, Designation) = (b'O' | DBCS, 2, D::Jisx0213_2000_1);
    const J2000_2: (u8, usize, Designation) = (b'P' | DBCS, 2, D::Jisx0213_2000_2);
    const J2004_1: (u8, usize, Designation) = (b'Q' | DBCS, 2, D::Jisx0213_2004_1);
    const J2004_2: (u8, usize, Designation) = (b'P' | DBCS, 2, D::Jisx0213_2004_2);
    const JP: u8 = NO_SHIFT | USE_JISX0208_EXT;
    match variant {
        Variant::Kr => (0, &[KSX1001]),
        Variant::Jp => (JP, &[JISX0208, JISX0201_R, JISX0208_O]),
        Variant::Jp1 => (JP, &[JISX0208, JISX0212, JISX0201_R, JISX0208_O]),
        Variant::Jp2 => {
            (JP | USE_G2, &[JISX0208, JISX0212, KSX1001, GB2312, JISX0201_R, JISX0208_O, ISO8859_1, ISO8859_7])
        }
        Variant::Jp2004 => (JP, &[J2004_1, JISX0208, J2004_1, J2004_2]),
        Variant::Jp3 => (JP, &[J2000_1, JISX0208, J2000_1, J2000_2]),
        Variant::JpExt => (JP, &[JISX0208, JISX0212, JISX0201_R, JISX0201_K, JISX0208_O]),
    }
}

/// A designation's decoder (`None` for an unmappable code).
fn designation_decode(designation: Designation, data: &[u8]) -> Option<u32> {
    let (d0, d1) = (data[0], data.get(1).copied().unwrap_or(0));
    let plane1 = |d0: u8, d1: u8| {
        if d0 == 0x21 && d1 == 0x40 {
            return Some(0xFF3C);
        }
        trymap(Map::Jisx0208, d0, d1)
            .or_else(|| trymap(Map::Jisx0213_1Bmp, d0, d1))
            .or_else(|| trymap(Map::Jisx0213_1Emp, d0, d1).map(|u| u | 0x20000))
            .or_else(|| trymap(Map::Jisx0213Pair, d0, d1))
    };
    let plane2 = |d0: u8, d1: u8| {
        trymap(Map::Jisx0213_2Bmp, d0, d1).or_else(|| trymap(Map::Jisx0213_2Emp, d0, d1).map(|u| u | 0x20000))
    };
    match designation {
        Designation::Ksx1001 => trymap(Map::Ksx1001, d0, d1),
        Designation::Jisx0208 => {
            if d0 == 0x21 && d1 == 0x40 {
                Some(0xFF3C)
            } else {
                trymap(Map::Jisx0208, d0, d1)
            }
        }
        Designation::Jisx0212 => trymap(Map::Jisx0212, d0, d1),
        Designation::Jisx0213_2000_1 => {
            if jisx0213_2000_invalid(d0, d1) {
                None
            } else {
                plane1(d0, d1)
            }
        }
        // The JIS X 0213:2000 emulation's value is overwritten by the lookups that follow it.
        Designation::Jisx0213_2000_2 | Designation::Jisx0213_2004_2 => plane2(d0, d1),
        Designation::Jisx0213_2004_1 => plane1(d0, d1),
        Designation::Gb2312 => trymap(Map::Gb2312, d0, d1),
        Designation::Jisx0201R => match d0 {
            0x5C => Some(0xA5),
            0x7E => Some(0x203E),
            0..=0x7F => Some(u32::from(d0)),
            _ => None,
        },
        Designation::Jisx0201K => {
            let c = d0 ^ 0x80;
            (0xA1..=0xDF).contains(&c).then(|| 0xFEC0 + u32::from(c))
        }
        Designation::Dummy => None,
    }
}

fn is_escend(c: u8) -> bool {
    c.is_ascii_uppercase() || c == b'@'
}

/// Processes one ISO-2022 escape sequence.
fn process_escape(
    flags: u8,
    designations: &[(u8, usize, Designation)],
    state: &mut State,
    data: &[u8],
    pos: &mut usize,
) -> Result<(), Stop> {
    let input = &data[*pos..];
    let left = input.len();
    let mut esclen = 0;
    let mut i = 1;
    while i < 16 {
        if i >= left {
            return Err(Stop::TooFew);
        }
        if is_escend(input[i]) {
            esclen = i + 1;
            break;
        } else if flags & USE_JISX0208_EXT != 0 && i + 1 < left && input[i] == b'&' && input[i + 1] == b'@' {
            i += 2;
        }
        i += 1;
    }
    let (charset, designation) = match esclen {
        0 => return Err(Stop::Illegal(1)),
        3 => {
            if input[1] == b'$' {
                (input[2] | DBCS, 0)
            } else {
                let designation = match input[1] {
                    b'(' => 0,
                    b')' => 1,
                    b'.' if flags & USE_G2 != 0 => 2,
                    _ => return Err(Stop::Illegal(3)),
                };
                (input[2], designation)
            }
        }
        4 => {
            if input[1] != b'$' {
                return Err(Stop::Illegal(4));
            }
            let designation = match input[2] {
                b'(' => 0,
                b')' => 1,
                _ => return Err(Stop::Illegal(4)),
            };
            (input[3] | DBCS, designation)
        }
        6 => {
            if flags & USE_JISX0208_EXT != 0 && input[3] == ESC && input[4] == b'$' && input[5] == b'B' {
                (b'B' | DBCS, 0)
            } else {
                return Err(Stop::Illegal(6));
            }
        }
        other => return Err(Stop::Illegal(other)),
    };
    if charset != CHARSET_ASCII && !designations.iter().any(|(mark, _, _)| *mark == charset) {
        return Err(Stop::Illegal(esclen));
    }
    state.0[designation] = charset;
    *pos += esclen;
    Ok(())
}

/// Decodes one ISO 8859-7 byte.
fn iso8859_7(c: u8) -> Option<u32> {
    let code = u32::from(c);
    if c < 0xA0 || (c < 0xC0 && 0x288F_3BC9u32 & (1 << (c - 0xA0)) != 0) {
        Some(code)
    } else if (0xB4..=0xFE).contains(&c) && (c >= 0xD4 || 0xBFFF_FD77u32 & (1 << (c - 0xB4)) != 0) {
        Some(0x02D0 + code)
    } else {
        match c {
            0xA1 => Some(0x2018),
            0xA2 => Some(0x2019),
            0xAF => Some(0x2015),
            _ => None,
        }
    }
}

/// Processes a G2 character (single shift 2, `ESC N`).
fn process_g2(state: &State, data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    let c = data[*pos + 2];
    let decoded = match state.0[2] {
        CHARSET_ISO8859_1 if c < 0x80 => u32::from(c) + 0x80,
        CHARSET_ISO8859_1 => return Err(Stop::Illegal(3)),
        CHARSET_ISO8859_7 => iso8859_7(c ^ 0x80).ok_or(Stop::Illegal(3))?,
        CHARSET_ASCII if c & 0x80 != 0 => return Err(Stop::Illegal(3)),
        CHARSET_ASCII => u32::from(c),
        _ => return Err(Stop::Internal),
    };
    out.push(decoded);
    *pos += 3;
    Ok(())
}

fn iso2022(variant: Variant, state: &mut State, data: &[u8], pos: &mut usize, out: &mut Vec<u32>) -> Result<(), Stop> {
    let (flags, designations) = config(variant);
    while *pos < data.len() {
        let c = data[*pos];
        if state.0[4] & F_ESCTHROUGHOUT != 0 {
            out.push(u32::from(c));
            *pos += 1;
            if is_escend(c) {
                state.0[4] &= !F_ESCTHROUGHOUT;
            }
            continue;
        }
        let bypass = match c {
            ESC => {
                require(data, *pos, 2)?;
                let c2 = data[*pos + 1];
                if matches!(c2, b'(' | b')' | b'$' | b'.' | b'&') {
                    process_escape(flags, designations, state, data, pos)?;
                } else if flags & USE_G2 != 0 && c2 == b'N' {
                    require(data, *pos, 3)?;
                    process_g2(state, data, pos, out)?;
                } else {
                    out.push(u32::from(ESC));
                    state.0[4] |= F_ESCTHROUGHOUT;
                    *pos += 1;
                }
                false
            }
            SI | SO if flags & NO_SHIFT != 0 => true,
            SI => {
                state.0[4] &= !F_SHIFTED;
                *pos += 1;
                false
            }
            SO => {
                state.0[4] |= F_SHIFTED;
                *pos += 1;
                false
            }
            LF => {
                state.0[4] &= !F_SHIFTED;
                out.push(u32::from(LF));
                *pos += 1;
                false
            }
            0..=0x1F => true,
            0x80.. => return Err(Stop::Illegal(1)),
            _ => {
                let charset = if state.0[4] & F_SHIFTED != 0 { state.0[1] } else { state.0[0] };
                if charset == CHARSET_ASCII {
                    true
                } else {
                    let &(_, width, designation) =
                        designations.iter().find(|(mark, _, _)| *mark == charset).ok_or(Stop::Internal)?;
                    require(data, *pos, width)?;
                    let decoded =
                        designation_decode(designation, &data[*pos..*pos + width]).ok_or(Stop::Illegal(width))?;
                    if decoded < 0x30000 {
                        out.push(decoded);
                    } else {
                        out.push(decoded >> 16);
                        out.push(decoded & 0xFFFF);
                    }
                    *pos += width;
                    false
                }
            }
        };
        if bypass {
            out.push(u32::from(c));
            *pos += 1;
        }
    }
    Ok(())
}

//! Text codec lookup and the codecs the patch modules can meet when a game file names its own encoding:
//! name normalization and aliases, the 256-character map given to the XML parser for an encoding it
//! does not know itself, and encoding for the codecs a text file can be written with.

use super::codec_data::{ByteTableInfo, ALIASES, MODULES, TABLES, TABLE_DATA};
use super::{pycjk, pyidna};

/// The result of looking a codec name up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The codec module implementing it, and whether it is a text encoding.
    Found { module: &'static str, text: bool },
    /// No such encoding.
    Unknown,
}

/// Looks a codec name up: normalization (ASCII letters and digits lower-cased, `.` kept, every other
/// run of characters one `_`, none leading), then aliases, then the module name itself.
pub fn lookup(name: &str) -> Lookup {
    if name.contains('\0') {
        return Lookup::Unknown;
    }
    let mut normalized = String::new();
    let mut punct = false;
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'.' {
            if punct && !normalized.is_empty() {
                normalized.push('_');
            }
            punct = false;
            normalized.push(char::from(byte.to_ascii_lowercase()));
        } else {
            punct = true;
        }
    }
    let alias = |key: &str| ALIASES.iter().find(|(alias, _)| *alias == key).map(|(_, module)| *module);
    let aliased = alias(&normalized).or_else(|| alias(&normalized.replace('.', "_")));
    let candidates = aliased.into_iter().chain([normalized.as_str()]);
    for candidate in candidates {
        if candidate.is_empty() || candidate.contains('.') {
            continue;
        }
        if let Some((module, text)) = MODULES.iter().find(|(module, _)| *module == candidate) {
            return Lookup::Found { module, text: *text };
        }
    }
    Lookup::Unknown
}

fn table(module: &str) -> Option<(&'static ByteTableInfo, [Option<char>; 256])> {
    let index = TABLES.iter().position(|info| info.name == module)?;
    let data = &TABLE_DATA[index * 512..(index + 1) * 512];
    let mut map = [None; 256];
    for (slot, pair) in map.iter_mut().zip(data.as_chunks::<2>().0) {
        let value = u16::from_le_bytes([pair[0], pair[1]]);
        *slot = (value != 0xFFFF).then(|| char::from_u32(u32::from(value))).flatten();
    }
    Some((&TABLES[index], map))
}

/// The 256 characters a single-byte codec gives each byte (`None` for an undefined byte): a table codec's
/// table, or the Windows ANSI/OEM code page tables `mbcs` and `oem` decode with.
pub(crate) fn byte_table(module: &str) -> Option<[Option<char>; 256]> {
    table(module).map(|(_, map)| map)
}

/// The byte map given to the XML parser for an encoding name it does not know itself,
/// `None` when the encoding is refused (unknown, multi-byte, or not
/// ASCII-compatible where XML needs it).
pub fn expat_byte_map(name: &str) -> Option<[Option<char>; 256]> {
    let Lookup::Found { module, text: true } = lookup(name) else {
        return None;
    };
    table(module).filter(|(info, _)| info.expat).map(|(_, map)| map)
}

/// The result of encoding a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoded {
    Bytes(Vec<u8>),
    /// A character the codec cannot encode (always, for the `undefined` codec).
    Unencodable,
}

/// What a text file in that encoding receives for one written text: a streaming
/// encode that is not finalized (a byte-order mark first for `utf_8_sig`, `utf_16` and `utf_32`; the
/// multi-byte, `idna` and `punycode` encoders as [`pycjk`] and [`pyidna`] describe).
pub fn encode(module: &str, text: &str) -> Encoded {
    let chars = text.chars();
    let bytes = |result: Option<Vec<u8>>| result.map_or(Encoded::Unencodable, Encoded::Bytes);
    match module {
        "utf_8" => Encoded::Bytes(text.as_bytes().to_vec()),
        "utf_8_sig" => Encoded::Bytes([&[0xEF, 0xBB, 0xBF], text.as_bytes()].concat()),
        "ascii" => bytes(chars.map(|c| u8::try_from(c).ok().filter(u8::is_ascii)).collect()),
        "latin_1" | "charmap" => bytes(chars.map(|c| u8::try_from(c).ok()).collect()),
        "utf_16" => {
            Encoded::Bytes([0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect())
        }
        "utf_16_le" => Encoded::Bytes(text.encode_utf16().flat_map(u16::to_le_bytes).collect()),
        "utf_16_be" => Encoded::Bytes(text.encode_utf16().flat_map(u16::to_be_bytes).collect()),
        "utf_32" => Encoded::Bytes(
            [0xFF, 0xFE, 0, 0].into_iter().chain(chars.flat_map(|c| u32::from(c).to_le_bytes())).collect(),
        ),
        "utf_32_le" => Encoded::Bytes(chars.flat_map(|c| u32::from(c).to_le_bytes()).collect()),
        "utf_32_be" => Encoded::Bytes(chars.flat_map(|c| u32::from(c).to_be_bytes()).collect()),
        "unicode_escape" => Encoded::Bytes(unicode_escape(text, false)),
        "raw_unicode_escape" => Encoded::Bytes(unicode_escape(text, true)),
        "undefined" => Encoded::Unencodable,
        "utf_7" => Encoded::Bytes(utf_7(text)),
        "idna" => bytes(pyidna::idna(text)),
        "punycode" => Encoded::Bytes(pyidna::punycode(&text.chars().collect::<Vec<_>>())),
        _ if pycjk::is_multibyte(module) => bytes(pycjk::encode(module, text)),
        _ => match table(module).filter(|(info, _)| info.charmap_encoder) {
            Some((info, map)) => bytes(chars.map(|c| charmap_byte(info, &map, c)).collect()),
            None => Encoded::Unencodable,
        },
    }
}

fn charmap_byte(info: &ByteTableInfo, map: &[Option<char>; 256], c: char) -> Option<u8> {
    let code = u32::from(c);
    if let Some((_, byte)) = info.encode_overrides.iter().find(|(char_code, _)| u32::from(*char_code) == code) {
        return Some(*byte);
    }
    map.iter().position(|mapped| *mapped == Some(c)).and_then(|index| u8::try_from(index).ok())
}

/// `utf_7` (set O and white space written directly, each written text encoded
/// as a whole): `+` as `+-`, other characters outside the direct set as
/// modified base64 of their UTF-16 units between `+` and an implicit or explicit `-` end.
fn utf_7(text: &str) -> Vec<u8> {
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let direct = |c: char| {
        c.is_ascii()
            && c != '\0'
            && (c.is_ascii_graphic() || matches!(c, ' ' | '\t' | '\n' | '\r'))
            && !matches!(c, '+' | '\\' | '~')
    };
    let mut out = Vec::new();
    let mut in_shift = false;
    let (mut bits, mut buffer) = (0u32, 0u64);
    for c in text.chars() {
        if in_shift && direct(c) {
            if bits > 0 {
                out.push(BASE64[((buffer << (6 - bits)) & 0x3f) as usize]);
                buffer = 0;
                bits = 0;
            }
            in_shift = false;
            if c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-') {
                out.push(b'-');
            }
            out.push(c as u8);
            continue;
        }
        if !in_shift {
            if c == '+' {
                out.extend_from_slice(b"+-");
                continue;
            }
            if direct(c) {
                out.push(c as u8);
                continue;
            }
            out.push(b'+');
            in_shift = true;
        }
        let mut units = [0u16; 2];
        for unit in c.encode_utf16(&mut units) {
            bits += 16;
            buffer = (buffer << 16) | u64::from(*unit);
            while bits >= 6 {
                out.push(BASE64[((buffer >> (bits - 6)) & 0x3f) as usize]);
                bits -= 6;
            }
        }
    }
    if bits > 0 {
        out.push(BASE64[((buffer << (6 - bits)) & 0x3f) as usize]);
    }
    if in_shift {
        out.push(b'-');
    }
    out
}

/// `unicode_escape` / `raw_unicode_escape` encoders.
fn unicode_escape(text: &str, raw: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for c in text.chars() {
        let code = u32::from(c);
        let escaped = match code {
            0x10000.. => format!("\\U{code:08x}"),
            0x100.. => format!("\\u{code:04x}"),
            _ if raw => {
                out.push(code as u8);
                continue;
            }
            0x5C => "\\\\".to_owned(),
            0x09 => "\\t".to_owned(),
            0x0A => "\\n".to_owned(),
            0x0D => "\\r".to_owned(),
            0x20..=0x7E => {
                out.push(code as u8);
                continue;
            }
            _ => format!("\\x{code:02x}"),
        };
        out.extend(escaped.bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Name normalization and aliases.
    #[test]
    fn lookups() {
        let module = |name: &str| match lookup(name) {
            Lookup::Found { module, .. } => Some(module),
            Lookup::Unknown => None,
        };
        assert_eq!(module("UTF-8"), Some("utf_8"));
        assert_eq!(module("utf8"), Some("utf_8"));
        assert_eq!(module(" Windows-1252 "), Some("cp1252"));
        assert_eq!(module("latin-1"), Some("latin_1"));
        assert_eq!(module("ISO-8859-1"), Some("latin_1"));
        assert_eq!(module("iso8859_15"), Some("iso8859_15"));
        assert_eq!(module("shift-jis"), Some("shift_jis"));
        assert_eq!(module("mbcs"), Some("mbcs"));
        assert_eq!(module("aliases"), None);
        assert_eq!(module("nope"), None);
        assert_eq!(module("utf\u{0}8"), None);
        assert_eq!(lookup("base64"), Lookup::Found { module: "base64_codec", text: false });
    }

    /// The byte map for codecs the XML parser does not know: cp1252 leaves its five undefined bytes invalid, "utf8"
    /// (not the parser's own "UTF-8") makes every non-ASCII byte invalid, EBCDIC and multi-byte codecs are refused.
    #[test]
    fn expat_maps() {
        let cp1252 = expat_byte_map("cp1252").unwrap();
        assert_eq!(cp1252[0x80], Some('\u{20ac}'));
        assert_eq!(cp1252[0x81], None);
        assert_eq!(cp1252[u8::try_from('<').unwrap() as usize], Some('<'));
        let utf8 = expat_byte_map("utf8").unwrap();
        assert_eq!(utf8[0x41], Some('A'));
        assert_eq!(utf8[0xC3], None);
        assert!(expat_byte_map("cp037").is_none());
        assert!(expat_byte_map("shift_jis").is_none());
        assert!(expat_byte_map("nope").is_none());
    }

    /// Encoded bytes per codec.
    #[test]
    fn encoders() {
        assert_eq!(encode("cp1252", "a\u{20ac}"), Encoded::Bytes(vec![b'a', 0x80]));
        assert_eq!(encode("cp1252", "\u{0101}"), Encoded::Unencodable);
        assert_eq!(encode("cp037", "A"), Encoded::Bytes(vec![0xC1]));
        assert_eq!(encode("utf_16", "a"), Encoded::Bytes(vec![0xFF, 0xFE, b'a', 0]));
        assert_eq!(encode("utf_8_sig", "a"), Encoded::Bytes(vec![0xEF, 0xBB, 0xBF, b'a']));
        assert_eq!(
            encode("unicode_escape", "\\\t\u{7f}\u{e9}\u{263a}\u{1f600}"),
            Encoded::Bytes(b"\\\\\\t\\x7f\\xe9\\u263a\\U0001f600".to_vec())
        );
        assert_eq!(encode("raw_unicode_escape", "\\\u{e9}\u{263a}"), Encoded::Bytes(b"\\\xe9\\u263a".to_vec()));
        assert_eq!(encode("shift_jis", "<a/>"), Encoded::Bytes(b"<a/>".to_vec()));
        assert_eq!(encode("shift_jis", "\u{3042}"), Encoded::Bytes(b"\x82\xa0".to_vec()));
        assert_eq!(encode("shift_jis", "\u{e9}"), Encoded::Unencodable);
        assert_eq!(encode("hz", "a~b"), Encoded::Bytes(b"a~~b".to_vec()));
        assert_eq!(encode("utf_7", "a+b"), Encoded::Bytes(b"a+-b".to_vec()));
        assert_eq!(
            encode("utf_7", "\u{e9}-\u{e9}a\u{e9}.\u{e9}+"),
            Encoded::Bytes(b"+AOk--+AOk-a+AOk.+AOkAKw-".to_vec())
        );
        assert_eq!(encode("utf_7", "\u{1f600}~\\"), Encoded::Bytes(b"+2D3eAAB+AFw-".to_vec()));
        assert_eq!(encode("idna", "a.\u{e9}."), Encoded::Bytes(b"a.xn--9ca.".to_vec()));
        assert_eq!(encode("punycode", "a\u{e9}"), Encoded::Bytes(b"a-bga".to_vec()));
        assert_eq!(encode("undefined", "a"), Encoded::Unencodable);
        assert_eq!(encode("mac_arabic", " "), Encoded::Bytes(vec![0xA0]));
    }
}

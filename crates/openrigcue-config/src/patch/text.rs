//! Reading and writing game text files in Windows text form.
//!
//! Reading decodes strictly and turns `\r\n` and `\r` into `\n` (universal newlines); writing turns every `\n`
//! into `\r\n` unless the caller asks for `\n` line ends. The default encoding is the Windows ANSI code page,
//! taken as Windows-1252.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Text encodings the patch modules open files with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// The default: the ANSI code page, Windows-1252.
    Ansi,
    /// UTF-8; a byte-order mark is kept as U+FEFF.
    Utf8,
    /// UTF-8 with signature: a leading byte-order mark is dropped.
    Utf8Sig,
    /// UTF-16: byte-order mark detection (little-endian without one), written with a little-endian mark.
    Utf16,
}

/// The decode error of a whole Windows-1252 read, before newline translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiReadDecodeError {
    input: Vec<u8>,
    start: usize,
    path: PathBuf,
}

impl AnsiReadDecodeError {
    /// The first undefined mapping, retaining all bytes consumed by the whole read.
    pub fn from_bytes(input: &[u8], path: &Path) -> Option<Self> {
        let start = input.iter().position(|byte| cp1252_char(*byte).is_none())?;
        Some(Self { input: input.to_vec(), start, path: path.to_owned() })
    }

    pub fn input(&self) -> &[u8] {
        &self.input
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn end(&self) -> usize {
        self.start + 1
    }

    pub fn codec(&self) -> &'static str {
        "charmap"
    }

    pub fn reason(&self) -> &'static str {
        "character maps to <undefined>"
    }

    /// The decode error's message text; the legacy API display is kept separately.
    pub fn native_message(&self) -> String {
        format!(
            "'{}' codec can't decode byte 0x{:02x} in position {}: {}",
            self.codec(),
            self.input[self.start],
            self.start,
            self.reason(),
        )
    }
}

impl std::fmt::Display for AnsiReadDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} is not valid Ansi text", self.path.display())
    }
}

impl std::error::Error for AnsiReadDecodeError {}

/// The decode error of a whole plain UTF-8 read, before newline translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utf8ReadDecodeError {
    input: Vec<u8>,
    start: usize,
    end: usize,
    reason: &'static str,
    path: PathBuf,
}

impl Utf8ReadDecodeError {
    pub fn from_bytes(input: &[u8], path: &Path) -> Option<Self> {
        let error = std::str::from_utf8(input).err()?;
        let start = error.valid_up_to();
        let end = error.error_len().map_or(input.len(), |length| start + length);
        let reason = if error.error_len().is_none() {
            "unexpected end of data"
        } else if matches!(input[start], 0xc2..=0xf4) {
            "invalid continuation byte"
        } else {
            "invalid start byte"
        };
        Some(Self { input: input.to_vec(), start, end, reason, path: path.to_owned() })
    }

    pub fn input(&self) -> &[u8] {
        &self.input
    }
    pub fn start(&self) -> usize {
        self.start
    }
    pub fn end(&self) -> usize {
        self.end
    }
    pub fn codec(&self) -> &'static str {
        "utf-8"
    }
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    pub fn native_message(&self) -> String {
        if self.end == self.start + 1 {
            format!(
                "'utf-8' codec can't decode byte 0x{:02x} in position {}: {}",
                self.input[self.start], self.start, self.reason
            )
        } else {
            format!("'utf-8' codec can't decode bytes in position {}-{}: {}", self.start, self.end - 1, self.reason)
        }
    }
}

impl std::fmt::Display for Utf8ReadDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} is not valid Utf8 text", self.path.display())
    }
}

impl std::error::Error for Utf8ReadDecodeError {}

/// Line-ending translation applied on write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Newline {
    /// Default text mode on Windows: `\n` becomes `\r\n`.
    Windows,
    /// `\n` line ends: written as is (no module writes text this way; Project Motor Racing writes
    /// encoded bytes itself).
    #[cfg_attr(not(test), allow(dead_code))]
    Unix,
}

/// Reads a whole text file.
pub fn read_text(path: &Path, encoding: Encoding) -> io::Result<String> {
    decode_text(&fs::read(path)?, path, encoding)
}

/// The text of a file holding `bytes`, read in text mode (`path` names the file in the
/// error).
pub fn decode_text(bytes: &[u8], path: &Path, encoding: Encoding) -> io::Result<String> {
    let text = decode(bytes, encoding).ok_or_else(|| match encoding {
        Encoding::Ansi => io::Error::new(
            io::ErrorKind::InvalidData,
            AnsiReadDecodeError::from_bytes(bytes, path).expect("undefined Windows-1252 mapping"),
        ),
        Encoding::Utf8 => io::Error::new(
            io::ErrorKind::InvalidData,
            Utf8ReadDecodeError::from_bytes(bytes, path).expect("invalid UTF-8"),
        ),
        _ => invalid_data(path, encoding),
    })?;
    Ok(universal_newlines(&text))
}

/// Writes a whole text file.
pub fn write_text(path: &Path, text: &str, encoding: Encoding, newline: Newline) -> io::Result<()> {
    let translated = match newline {
        Newline::Windows => text.replace('\n', "\r\n"),
        Newline::Unix => text.to_owned(),
    };
    let bytes = encode(&translated, encoding).ok_or_else(|| invalid_data(path, encoding))?;
    fs::write(path, bytes)
}

/// Writes a text file chunk by chunk: the file is truncated first; a chunk that cannot be encoded (`None`: a
/// lone surrogate, or a character the encoding lacks) is an error, and the chunks before it stay in the file.
pub fn write_chunks(
    path: &Path,
    chunks: impl IntoIterator<Item = Option<String>>,
    encoding: Encoding,
    newline: Newline,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    if encoding == Encoding::Utf16 {
        bytes.extend_from_slice(&[0xff, 0xfe]);
    }
    let mut failed = false;
    for chunk in chunks {
        let encoded = chunk.and_then(|text| {
            let translated = match newline {
                Newline::Windows => text.replace('\n', "\r\n"),
                Newline::Unix => text,
            };
            encode_without_mark(&translated, encoding)
        });
        match encoded {
            Some(chunk) => bytes.extend(chunk),
            None => {
                failed = true;
                break;
            }
        }
    }
    // The UTF-16 encoder writes its byte-order mark with the first chunk it encodes.
    if encoding == Encoding::Utf16 && bytes.len() == 2 {
        bytes.clear();
    }
    fs::write(path, bytes)?;
    if failed {
        return Err(invalid_data(path, encoding));
    }
    Ok(())
}

/// Splits text into lines that keep their `\n`.
pub fn lines_with_ends(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Whether one character is Unicode whitespace.
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Trims Unicode whitespace from both ends.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

fn invalid_data(path: &Path, encoding: Encoding) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{} is not valid {encoding:?} text", path.display()))
}

fn universal_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Decodes bytes; `None` when they do not decode.
pub fn decode(bytes: &[u8], encoding: Encoding) -> Option<String> {
    match encoding {
        Encoding::Ansi => bytes.iter().map(|byte| cp1252_char(*byte)).collect(),
        Encoding::Utf8 => String::from_utf8(bytes.to_vec()).ok(),
        Encoding::Utf8Sig => String::from_utf8(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes).to_vec()).ok(),
        Encoding::Utf16 => decode_utf16(bytes),
    }
}

fn encode(text: &str, encoding: Encoding) -> Option<Vec<u8>> {
    match encoding {
        Encoding::Utf16 => {
            let mut bytes = vec![0xff, 0xfe];
            bytes.extend(encode_without_mark(text, encoding)?);
            Some(bytes)
        }
        other => encode_without_mark(text, other),
    }
}

fn encode_without_mark(text: &str, encoding: Encoding) -> Option<Vec<u8>> {
    match encoding {
        Encoding::Ansi => text.chars().map(cp1252_byte).collect(),
        Encoding::Utf8 | Encoding::Utf8Sig => Some(text.as_bytes().to_vec()),
        Encoding::Utf16 => Some(text.encode_utf16().flat_map(u16::to_le_bytes).collect()),
    }
}

fn decode_utf16(bytes: &[u8]) -> Option<String> {
    let (big_endian, body) = match bytes {
        [0xfe, 0xff, rest @ ..] => (true, rest),
        [0xff, 0xfe, rest @ ..] => (false, rest),
        _ => (false, bytes),
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units = body.as_chunks::<2>().0.iter().map(|pair| {
        let pair = [pair[0], pair[1]];
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    });
    char::decode_utf16(units).collect::<Result<String, _>>().ok()
}

/// Windows-1252 bytes 0x80–0x9F; `None` marks the five undefined bytes.
const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20ac}'),
    None,
    Some('\u{201a}'),
    Some('\u{0192}'),
    Some('\u{201e}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02c6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017d}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201c}'),
    Some('\u{201d}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02dc}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203a}'),
    Some('\u{0153}'),
    None,
    Some('\u{017e}'),
    Some('\u{0178}'),
];

fn cp1252_char(byte: u8) -> Option<char> {
    match byte {
        0x80..=0x9f => CP1252_HIGH[usize::from(byte - 0x80)],
        _ => Some(char::from(byte)),
    }
}

fn cp1252_byte(c: char) -> Option<u8> {
    let code = u32::from(c);
    if code < 0x80 || (0xa0..=0xff).contains(&code) {
        return u8::try_from(code).ok();
    }
    let index = CP1252_HIGH.iter().position(|mapped| *mapped == Some(c))?;
    u8::try_from(0x80 + index).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_utf8_whole_read_keeps_error_details_and_legacy_display() {
        let input = b"valid\r\n\xe2\x82(";
        let error = decode_text(input, Path::new("SRSExport.lua"), Encoding::Utf8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "SRSExport.lua is not valid Utf8 text");
        let native = error.get_ref().unwrap().downcast_ref::<Utf8ReadDecodeError>().unwrap();
        assert_eq!(native.input(), input);
        assert_eq!((native.start(), native.end()), (7, 9));
        assert_eq!(native.codec(), "utf-8");
        assert_eq!(native.reason(), "invalid continuation byte");
        assert_eq!(
            crate::patch::PatchError::from(error).native_exception(),
            Some((
                "UnicodeDecodeError",
                "'utf-8' codec can't decode bytes in position 7-8: invalid continuation byte".into(),
            ))
        );
    }

    #[test]
    fn native_utf8_error_spans_reasons_and_full_payload_match_cpython() {
        let cases: &[(&[u8], usize, &str)] = &[
            (b"\xff", 1, "invalid start byte"),
            (b"\x80", 1, "invalid start byte"),
            (b"\xc0\x80", 1, "invalid start byte"),
            (b"\xf5\x80\x80\x80", 1, "invalid start byte"),
            (b"\xc2", 1, "unexpected end of data"),
            (b"\xe2\x82", 2, "unexpected end of data"),
            (b"\xf0\x9f\x92", 3, "unexpected end of data"),
            (b"\xe2(", 1, "invalid continuation byte"),
            (b"\xe2\x82(", 2, "invalid continuation byte"),
            (b"\xf0\x9f\x92(", 3, "invalid continuation byte"),
            (b"\xed\xa0\x80", 1, "invalid continuation byte"),
            (b"\xe0\x80\x80", 1, "invalid continuation byte"),
            (b"\xf4\x90\x80\x80", 1, "invalid continuation byte"),
        ];
        for prefix in [b"".to_vec(), b"\xef\xbb\xbfok\r\n".to_vec(), vec![b'a'; 16384]] {
            for (invalid, length, reason) in cases {
                let mut input = prefix.clone();
                input.extend_from_slice(invalid);
                let error = decode_text(&input, Path::new("fixture.lua"), Encoding::Utf8).unwrap_err();
                let native = error.get_ref().unwrap().downcast_ref::<Utf8ReadDecodeError>().unwrap();
                assert_eq!(native.input(), input);
                assert_eq!(
                    (native.start(), native.end(), native.reason()),
                    (prefix.len(), prefix.len() + length, *reason)
                );
                let expected = if *length == 1 {
                    format!(
                        "'utf-8' codec can't decode byte 0x{:02x} in position {}: {reason}",
                        invalid[0],
                        prefix.len()
                    )
                } else {
                    format!(
                        "'utf-8' codec can't decode bytes in position {}-{}: {reason}",
                        prefix.len(),
                        prefix.len() + length - 1
                    )
                };
                assert_eq!(native.native_message(), expected);
            }
        }
        let input = b"first\r\n\xfftail\xe2\x82";
        let error = Utf8ReadDecodeError::from_bytes(input, Path::new("first.lua")).unwrap();
        assert_eq!(error.input(), input);
        assert_eq!((error.start(), error.end(), error.reason()), (7, 8, "invalid start byte"));
    }

    #[test]
    fn native_utf8_metadata_is_plain_read_only_and_valid_text_keeps_bom_and_newlines() {
        use crate::patch::PatchError;
        let path = Path::new("fixture.lua");
        assert!(Utf8ReadDecodeError::from_bytes("\u{feff}café\r\n".as_bytes(), path).is_none());
        assert_eq!(
            decode_text("\u{feff}café\r\nx\ry\n".as_bytes(), path, Encoding::Utf8).unwrap(),
            "\u{feff}café\nx\ny\n"
        );
        for error in [
            invalid_data(path, Encoding::Utf8),
            decode_text(b"\xff", path, Encoding::Utf8Sig).unwrap_err(),
            decode_text(b"\xff", path, Encoding::Utf16).unwrap_err(),
            io::Error::new(
                io::ErrorKind::InvalidData,
                "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte",
            ),
            io::Error::new(io::ErrorKind::PermissionDenied, "fixture access denied"),
        ] {
            assert!(PatchError::from(error).native_exception().is_none());
        }
    }

    #[test]
    fn native_cp1252_whole_read_retains_the_payload_and_file_relative_span() {
        let path = Path::new("fixture/Game.ini");
        for byte in [0x81, 0x8d, 0x8f, 0x90, 0x9d] {
            for start in [0, 8191, 8192, 8209, 16384] {
                let mut input = vec![b'a'; start];
                input.push(byte);
                input.extend_from_slice(b"TAIL\r\n");
                let error = decode_text(&input, path, Encoding::Ansi).unwrap_err();
                let native = error.get_ref().and_then(|error| error.downcast_ref::<AnsiReadDecodeError>()).unwrap();
                assert_eq!(native.input(), input);
                assert_eq!(native.start(), start);
                assert_eq!(native.end(), start + 1);
                assert_eq!(native.codec(), "charmap");
                assert_eq!(native.reason(), "character maps to <undefined>");
                assert_eq!(native.native_message(), format!("'charmap' codec can't decode byte 0x{byte:02x} in position {start}: character maps to <undefined>"));
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert_eq!(error.to_string(), "fixture/Game.ini is not valid Ansi text");
                let error = crate::patch::PatchError::from(error);
                assert_eq!(error.native_exception(), Some(("UnicodeDecodeError", native_message(byte, start))));
            }
        }
    }

    fn native_message(byte: u8, start: usize) -> String {
        format!("'charmap' codec can't decode byte 0x{byte:02x} in position {start}: character maps to <undefined>")
    }

    #[test]
    fn native_cp1252_whole_read_reports_only_the_first_bad_byte_before_newlines_are_translated() {
        let input = b"good\r\nline\r\x81\x8d\x90TAIL\r\n";
        let error = decode_text(input, Path::new("Game.ini"), Encoding::Ansi).unwrap_err();
        let native = error.get_ref().and_then(|error| error.downcast_ref::<AnsiReadDecodeError>()).unwrap();
        assert_eq!(native.input(), input);
        assert_eq!((native.start(), native.end()), (11, 12));
        assert_eq!(native.native_message(), native_message(0x81, 11));
    }

    #[test]
    fn native_cp1252_error_metadata_requires_a_typed_read_error() {
        use crate::patch::PatchError;

        assert!(AnsiReadDecodeError::from_bytes(b"valid\r\n\x80\xe9", Path::new("Game.ini")).is_none());
        for error in [
            invalid_data(Path::new("write.ini"), Encoding::Ansi),
            decode_text(b"\xff", Path::new("utf8sig.ini"), Encoding::Utf8Sig).unwrap_err(),
            io::Error::new(io::ErrorKind::InvalidData, native_message(0x81, 0)),
            io::Error::new(io::ErrorKind::PermissionDenied, "fixture access denied"),
        ] {
            assert!(PatchError::from(error).native_exception().is_none());
        }
        assert!(PatchError::MissingElement.native_exception().is_none());
    }

    #[test]
    fn native_cp1252_valid_whole_reads_keep_selected_codepage_and_newline_behavior() {
        let path = Path::new("Game.ini");
        assert_eq!(decode_text(b"", path, Encoding::Ansi).unwrap(), "");
        assert_eq!(
            decode_text(b"\x00\x80\x82\x8e\x9f\xa0\xe9\xff\r\none\rtwo\n", path, Encoding::Ansi).unwrap(),
            "\0\u{20ac}\u{201a}\u{17d}\u{178}\u{a0}\u{e9}\u{ff}\none\ntwo\n"
        );
        assert_eq!(decode_text(b"\xef\xbb\xbf", path, Encoding::Ansi).unwrap(), "\u{ef}\u{bb}\u{bf}");
        assert_eq!(encode("\u{20ac}\u{e9}\r\n", Encoding::Ansi).unwrap(), b"\x80\xe9\r\n");
        assert!(decode(b"\x81", Encoding::Ansi).is_none());
    }

    /// Text mode: universal newlines on read, CRLF on write, strict code-page decoding.
    #[test]
    fn windows_text_mode_round_trip() {
        let dir = std::env::temp_dir().join(format!("openrigcue-config-b-text-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        fs::write(&path, b"a\r\nb\rc\n\x80\xe9").unwrap();
        assert_eq!(read_text(&path, Encoding::Ansi).unwrap(), "a\nb\nc\n\u{20ac}\u{e9}");
        write_text(&path, "x\ny\n\u{20ac}", Encoding::Ansi, Newline::Windows).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"x\r\ny\r\n\x80");
        write_text(&path, "x\n", Encoding::Utf8, Newline::Unix).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"x\n");
        fs::write(&path, b"\x81").unwrap();
        assert!(read_text(&path, Encoding::Ansi).is_err());
        assert!(write_text(&path, "\u{3042}", Encoding::Ansi, Newline::Windows).is_err());
    }

    /// utf-8 keeps a BOM as U+FEFF, utf-8-sig drops it.
    #[test]
    fn utf8_bom_handling() {
        assert_eq!(decode(b"\xef\xbb\xbfk", Encoding::Utf8).unwrap(), "\u{feff}k");
        assert_eq!(decode(b"\xef\xbb\xbfk", Encoding::Utf8Sig).unwrap(), "k");
    }

    /// UTF-16: BOM detection, little-endian default, odd length and lone surrogates fail.
    #[test]
    fn utf16_like_python() {
        assert_eq!(decode(b"\xff\xfea\x00", Encoding::Utf16).unwrap(), "a");
        assert_eq!(decode(b"\xfe\xff\x00a", Encoding::Utf16).unwrap(), "a");
        assert_eq!(decode(b"ab", Encoding::Utf16).unwrap(), "\u{6261}");
        assert_eq!(decode(b"abc", Encoding::Utf16), None);
        assert_eq!(decode(b"\x00\xd8", Encoding::Utf16), None);
        assert_eq!(encode("a", Encoding::Utf16).unwrap(), b"\xff\xfea\x00");
    }

    /// Line splitting keeps line ends; trimming uses Unicode whitespace.
    #[test]
    fn lines_and_strip() {
        assert_eq!(lines_with_ends("a\nb"), ["a\n", "b"]);
        assert_eq!(py_strip("\u{1f} x \n"), "x");
    }
}

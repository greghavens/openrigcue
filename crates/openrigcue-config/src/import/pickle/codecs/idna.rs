//! `idna` and `punycode` decoding: only the `strict` error handler is supported for `idna` (others
//! are a `UnicodeError`), each label goes through ToUnicode (Punycode, then ToASCII with nameprep for
//! the round-trip check), and Punycode arithmetic uses unbounded integers.
//!
//! Each wrapped error keeps its chain: the new `UnicodeDecodeError` (its object and offsets), the
//! underlying error as its context (hidden for Punycode digit errors), and the registry note
//! of the inner Punycode decode of a label.

use super::{error, with_codec_note, NativeException};
use crate::import::value::BigInt;
use crate::patch::pyidna::{self, NameprepError};
use num_traits::{ToPrimitive, Zero};

fn overflow() -> NativeException {
    error("OverflowError", "Python int too large to convert to C ssize_t")
}

fn unsupported(errors: &str) -> NativeException {
    error("UnicodeError", format!("Unsupported error handling: {errors}"))
}

fn is_class(exception: &NativeException, classes: &[&str]) -> bool {
    classes.contains(&exception.class())
}

/// A `UnicodeDecodeError` for `object` with the range of `exc` shifted by `offset`, raised in the context of
/// `exc` (context hidden when `suppress`): an offset beyond the signed index range is an `OverflowError`
/// instead, whose context is never hidden.
fn reraise(encoding: &str, object: &[u8], offset: i64, inner: NativeException, suppress: bool) -> NativeException {
    let attributes = inner.unicode_error().expect("a UnicodeError");
    let (start, end) = (offset.checked_add(attributes.start), offset.checked_add(attributes.end));
    let reason = attributes.reason.scalar_string().expect("codec reasons are scalar text");
    match (start, end) {
        (Some(start), Some(end)) => {
            NativeException::unicode_decode(encoding, object, start, end, &reason).with_context(inner, suppress)
        }
        _ => overflow().with_context(inner, false),
    }
}

/// The quoted, escaped literal form of a byte string.
fn bytes_repr(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = format!("b{}", char::from(quote));
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if byte == quote => {
                out.push('\\');
                out.push(char::from(quote));
            }
            0x20..=0x7E => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push(char::from(quote));
    out
}

/// The quoted form of one character nameprep prohibits: of tables C.1.2–C.9 (as NFKC leaves them) only U+FFFC,
/// U+FFFD and U+2FF0–U+2FFB are printable.
fn char_repr(c: u32) -> String {
    if c == 0xFFFC || c == 0xFFFD || (0x2FF0..=0x2FFB).contains(&c) {
        return format!("'{}'", char::from_u32(c).expect("printable"));
    }
    match c {
        0..=0xFF => format!("'\\x{c:02x}'"),
        0x100..=0xFFFF => format!("'\\u{c:04x}'"),
        _ => format!("'\\U{c:08x}'"),
    }
}

/// ToASCII of one label; errors are `idna` `UnicodeEncodeError`s of the label in hand.
fn to_ascii(label: &[u32]) -> Result<Vec<u8>, NativeException> {
    let encode = |label: &[u32], start: usize, end: usize, reason: &str| {
        NativeException::unicode_encode("idna", label, start as i64, end as i64, reason)
    };
    let ascii =
        |label: &[u32]| label.iter().all(|c| *c < 128).then(|| label.iter().map(|c| *c as u8).collect::<Vec<u8>>());
    let checked = |label: &[u32], bytes: Vec<u8>| {
        if !label.is_empty() && label.len() < 64 {
            Ok(bytes)
        } else if label.is_empty() {
            Err(encode(label, 0, 1, "label empty"))
        } else {
            Err(encode(label, 0, label.len(), "label too long"))
        }
    };
    if let Some(bytes) = ascii(label) {
        return checked(label, bytes);
    }
    let label = pyidna::nameprep_code_points(label).map_err(|(normalized, failure)| match failure {
        NameprepError::Prohibited(index) => {
            let reason = format!("Invalid character {}", char_repr(normalized[index]));
            encode(&normalized, index, index + 1, &reason)
        }
        NameprepError::Bidi2(index) => encode(&normalized, index, index + 1, "Violation of BIDI requirement 2"),
        NameprepError::Bidi3(true) => encode(&normalized, 0, 1, "Violation of BIDI requirement 3"),
        NameprepError::Bidi3(false) => {
            let len = normalized.len();
            encode(&normalized, len - 1, len, "Violation of BIDI requirement 3")
        }
    })?;
    if let Some(bytes) = ascii(&label) {
        return checked(&label, bytes);
    }
    let lowered: String =
        label.iter().filter_map(|c| char::from_u32(*c)).take(4).flat_map(char::to_lowercase).collect();
    if lowered.starts_with("xn--") {
        return Err(encode(&label, 0, 4, "Label starts with ACE prefix"));
    }
    let chars: Vec<char> = label.iter().map(|c| char::from_u32(*c).expect("nameprep prohibits surrogates")).collect();
    let mut bytes = b"xn--".to_vec();
    bytes.extend(pyidna::punycode(&chars));
    if bytes.len() < 64 {
        Ok(bytes)
    } else {
        Err(encode(&label, 0, label.len(), "label too long"))
    }
}

/// ASCII decoding of `data` with `errors` (a failure carries no codec note).
fn ascii_text(data: &[u8], mode: Mode) -> Result<Vec<u32>, NativeException> {
    let mut out = Vec::with_capacity(data.len());
    for (index, byte) in data.iter().enumerate() {
        match (byte.is_ascii(), mode) {
            (true, _) => out.push(u32::from(*byte)),
            (false, Mode::Strict) => {
                return Err(NativeException::unicode_decode(
                    "ascii",
                    data,
                    index as i64,
                    index as i64 + 1,
                    "ordinal not in range(128)",
                ));
            }
            (false, Mode::Replace) => out.push(0xFFFD),
            (false, Mode::Ignore) => {}
        }
    }
    Ok(out)
}

/// ToUnicode of a byte label.
fn to_unicode(label: &[u8]) -> Result<Vec<u32>, NativeException> {
    if label.len() > 1024 {
        return Err(NativeException::unicode_decode("idna", label, 0, label.len() as i64, "label way too long"));
    }
    if !label.to_ascii_lowercase().starts_with(b"xn--") {
        return ascii_text(label, Mode::Strict);
    }
    let label1 = &label[4..];
    // Punycode decode of the label: empty bytes decode without the codec; a codec failure carries its note.
    let result = if label1.is_empty() {
        Vec::new()
    } else {
        match punycode_decode(label1, Mode::Strict) {
            Ok(result) => result,
            Err(inner) => {
                let inner = with_codec_note(inner, "punycode");
                if is_class(&inner, &["UnicodeDecodeError"]) {
                    return Err(reraise("idna", label, 4, inner, false));
                }
                return Err(inner);
            }
        }
    };
    let label2 = to_ascii(&result)?;
    // Both are ASCII here: Punycode accepted the label.
    if label.to_ascii_lowercase() != label2 {
        let reason = format!("IDNA does not round-trip, '{}' != '{}'", bytes_repr(label), bytes_repr(&label2));
        return Err(NativeException::unicode_decode("idna", label, 0, label.len() as i64, &reason));
    }
    Ok(result)
}

/// Decodes `input` as `idna`.
pub(super) fn idna(input: &[u8], errors: Option<&str>) -> Result<Vec<u32>, NativeException> {
    let errors = errors.unwrap_or("strict");
    if errors != "strict" {
        return Err(unsupported(errors));
    }
    let lowered = input.to_ascii_lowercase();
    if !lowered.windows(4).any(|w| w == b"xn--") && input.is_ascii() {
        return Ok(input.iter().map(|b| u32::from(*b)).collect());
    }
    let mut labels: Vec<&[u8]> = input.split(|b| *b == b'.').collect();
    let trailing_dot = labels.last().is_some_and(|label| label.is_empty());
    if trailing_dot {
        labels.pop();
    }
    let mut result = Vec::new();
    let mut offset = 0i64;
    for (index, label) in labels.iter().enumerate() {
        if index > 0 {
            result.push(u32::from(b'.'));
        }
        match to_unicode(label) {
            Ok(decoded) => result.extend(decoded),
            Err(inner) if is_class(&inner, &["UnicodeEncodeError", "UnicodeDecodeError"]) => {
                return Err(reraise("idna", input, offset, inner, false));
            }
            Err(other) => return Err(other),
        }
        offset += label.len() as i64 + 1;
    }
    if trailing_dot {
        result.push(u32::from(b'.'));
    }
    Ok(result)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Strict,
    Replace,
    Ignore,
}

/// Decodes `input` as `punycode`.
pub(super) fn punycode(input: &[u8], errors: Option<&str>) -> Result<Vec<u32>, NativeException> {
    let mode = match errors.unwrap_or("strict") {
        "strict" => Mode::Strict,
        "replace" => Mode::Replace,
        "ignore" => Mode::Ignore,
        other => return Err(unsupported(other)),
    };
    punycode_decode(input, mode)
}

/// The Punycode threshold for digit position `j` under `bias`.
fn threshold(j: i64, bias: i64) -> i64 {
    (36 * (j + 1) - bias).clamp(1, 26)
}

/// Punycode bias adaptation.
fn adapt(delta: &BigInt, first: bool, numchars: usize) -> i64 {
    let mut delta: BigInt = if first { delta / 700 } else { delta / 2 };
    let extra = &delta / BigInt::from(numchars);
    delta += extra;
    let mut divisions = 0i64;
    while delta > BigInt::from(455) {
        delta /= 35;
        divisions += 36;
    }
    let delta = delta.to_i64().expect("at most 455");
    divisions + 36 * delta / (delta + 38)
}

/// Decodes one generalized variable-length integer: the next position and the number, `None` after a
/// non-strict error. Errors are `punycode` `UnicodeDecodeError`s of the extended part.
fn generalized_number(
    extended: &[u8],
    mut extpos: usize,
    bias: i64,
    mode: Mode,
) -> Result<(usize, Option<BigInt>), NativeException> {
    let mut result = BigInt::zero();
    let mut w = BigInt::from(1);
    let mut j = 0i64;
    loop {
        let Some(&char) = extended.get(extpos) else {
            if mode == Mode::Strict {
                let (start, end) = (extpos as i64, extpos as i64 + 1);
                return Err(NativeException::unicode_decode(
                    "punycode",
                    extended,
                    start,
                    end,
                    "incomplete punycode string",
                ));
            }
            return Ok((extpos + 1, None));
        };
        extpos += 1;
        let digit = match char {
            0x41..=0x5A => i64::from(char - 0x41),
            0x30..=0x39 => i64::from(char) - 22,
            _ if mode == Mode::Strict => {
                let reason = format!("Invalid extended code point '{char}'");
                let (start, end) = (extpos as i64 - 1, extpos as i64);
                return Err(NativeException::unicode_decode("punycode", extended, start, end, &reason));
            }
            _ => return Ok((extpos, None)),
        };
        let t = threshold(j, bias);
        result += &w * digit;
        if digit < t {
            return Ok((extpos, Some(result)));
        }
        w *= 36 - t;
        j += 1;
    }
}

/// Inserts the code points the extended part encodes into the basic string.
fn insertion_sort(mut base: Vec<u32>, extended: &[u8], mode: Mode) -> Result<Vec<u32>, NativeException> {
    let mut char = BigInt::from(0x80);
    let mut pos = BigInt::from(-1);
    let mut bias = 72i64;
    let mut extpos = 0usize;
    while extpos < extended.len() {
        let (newpos, delta) = generalized_number(extended, extpos, bias, mode)?;
        let Some(delta) = delta else {
            return Ok(base);
        };
        pos += &delta + 1;
        let width = BigInt::from(base.len() + 1);
        char += &pos / &width;
        if char > BigInt::from(0x10FFFF) {
            if mode == Mode::Strict {
                let end = pos.to_i64().ok_or_else(overflow)?;
                let start = (&pos - BigInt::from(1)).to_i64().ok_or_else(overflow)?;
                let reason = format!("Invalid character U+{char:x}");
                return Err(NativeException::unicode_decode("punycode", extended, start, end, &reason));
            }
            char = BigInt::from(u32::from(b'?'));
        }
        pos = &pos % &width;
        let at = pos.to_usize().expect("below the base length");
        base.insert(at, char.to_u32().expect("checked code point"));
        bias = adapt(&delta, extpos == 0, base.len());
        extpos = newpos;
    }
    Ok(base)
}

/// Decodes one Punycode string.
fn punycode_decode(text: &[u8], mode: Mode) -> Result<Vec<u32>, NativeException> {
    let pos = text.iter().rposition(|b| *b == b'-');
    let (base, extended) = match pos {
        None => (Vec::new(), text.to_ascii_uppercase()),
        Some(pos) => {
            let base = match ascii_text(&text[..pos], mode) {
                Ok(base) => base,
                Err(inner) => return Err(reraise("ascii", text, 0, inner, true)),
            };
            (base, text[pos + 1..].to_ascii_uppercase())
        }
    };
    let offset = pos.map_or(0, |pos| pos as i64 + 1);
    insertion_sort(base, &extended, mode).map_err(|inner| {
        if is_class(&inner, &["UnicodeDecodeError"]) {
            reraise("punycode", text, offset, inner, true)
        } else {
            inner
        }
    })
}

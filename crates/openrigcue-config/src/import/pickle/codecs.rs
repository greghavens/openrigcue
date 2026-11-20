//! The pickle global `builtins.str` as a pickle can call it (`REDUCE`, `NEWOBJ`, `NEWOBJ_EX`), with every
//! standard text codec a pickled `str(bytes, encoding)` call can name.
//!
//! - Arguments are `object`, `encoding`, `errors`, all positional-or-keyword: the count, duplicate,
//!   unknown-keyword (with a "Did you mean" suggestion) and non-string-keyword errors come first, then
//!   the `encoding`/`errors` type, UTF-8 and embedded-NUL checks, then the conversion.
//! - `str(x)` returns an exact `str` argument itself (the same memoized object) and otherwise its text form;
//!   `str(x, encoding, errors)` decodes `bytes`, `bytearray` and `memoryview`; empty bytes give `''`
//!   without looking the codec up, and the common codecs are handled before the codec registry.
//! - Codec names resolve through `patch::pycodecs` (aliases and codec modules); binary codecs fail
//!   with the "not a text encoding" lookup error.
//! - Every decoder reports a `UnicodeDecodeError` (codec name, byte object, range, reason), and
//!   the built-in error handlers (`strict`, `ignore`, `replace`, `backslashreplace`, `surrogateescape`,
//!   `surrogatepass`, and the encode-only `xmlcharrefreplace`/`namereplace`) apply; an unknown handler
//!   name fails only when a decoding error needs it. Lone surrogates the codecs or
//!   handlers produce are kept (`PythonString`).
//! - `mbcs` and `oem` decode with Windows code pages 1252 and 437 (as the
//!   game-patch text files also assume); both map every byte.
//!
//! - Errors are [`NativeException`]s: the message as code points (an unexpected keyword holding lone
//!   surrogates keeps them), the `UnicodeError` attributes, the note line ("decoding with 'x' codec
//!   failed") added to whatever a registry codec reports, and the context chains
//!   of the `idna` / `punycode` errors.

use super::{Dict, Key, LegacyValue, PickleError, PythonString};
use crate::patch::pycodecs::{self, Lookup};
use openrigcue_model::native_value::NativeKey;

pub use exception::{backslashreplace, utf8_strict, NativeException, UnicodeErrorAttributes, UnicodeErrorObject};

mod cjk;
mod escape;
mod exception;
mod idna;
mod names;
#[cfg(test)]
mod tests;
mod text;

const KEYWORDS: [&str; 3] = ["object", "encoding", "errors"];

fn error(class: &'static str, message: impl AsRef<str>) -> NativeException {
    NativeException::plain(class, message)
}

/// `str(*args, **kwargs)`.
pub(super) fn construct_str(args: &[LegacyValue], kwargs: Option<&Dict>) -> Result<LegacyValue, PickleError> {
    str_new(args, kwargs).map_err(PickleError::Native)
}

/// The `str` constructor, returning its full error value on failure.
fn str_new(args: &[LegacyValue], kwargs: Option<&Dict>) -> Result<LegacyValue, NativeException> {
    let [object, encoding, errors] = unpack(args, kwargs)?;
    let encoding = encoding.map(|value| text_argument(value, "encoding")).transpose()?;
    let errors = errors.map(|value| text_argument(value, "errors")).transpose()?;
    let Some(object) = object else {
        return Ok(LegacyValue::Str(String::new()));
    };
    if encoding.is_none() && errors.is_none() {
        return Ok(match object.resolved() {
            LegacyValue::Str(_) | LegacyValue::Unicode(_) => object.clone(),
            _ => LegacyValue::from_python_string(openrigcue_model::python_repr::str_native(object).map_err(
                |detail| {
                    let (class, message) = detail.split_once(": ").unwrap_or(("ValueError", detail.as_str()));
                    error(if class == "OverflowError" { "OverflowError" } else { "ValueError" }, message)
                },
            )?),
        });
    }
    let resolved = object.resolved();
    let bytes = match (resolved.buffer_bytes(), &resolved) {
        (Some(bytes), _) => bytes,
        (None, LegacyValue::Str(_) | LegacyValue::Unicode(_)) => {
            return Err(error("TypeError", "decoding str is not supported"));
        }
        (None, other) => {
            return Err(error(
                "TypeError",
                format!("decoding to str: need a bytes-like object, {} found", tp_name(other)),
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(LegacyValue::Str(String::new()));
    }
    let decoded = decode(&bytes, encoding.as_deref(), errors.as_deref())?;
    let decoded = PythonString::from_codepoints(decoded).map_err(|message| error("ValueError", message))?;
    Ok(LegacyValue::from_python_string(decoded))
}

/// The type name of a value, as used in error messages.
fn tp_name(value: &LegacyValue) -> &'static str {
    value.qualified_type_name()
}

/// Binds positional and keyword arguments to the three parameters of `str` (zero to three accepted).
fn unpack<'a>(
    args: &'a [LegacyValue],
    kwargs: Option<&'a Dict>,
) -> Result<[Option<&'a LegacyValue>; 3], NativeException> {
    let nargs = args.len();
    let kwargs = kwargs.filter(|dict| !dict.is_empty());
    let mut slots = [None; 3];
    let Some(kwargs) = kwargs else {
        if nargs > 3 {
            return Err(error("TypeError", format!("str() takes at most 3 arguments ({nargs} given)")));
        }
        for (slot, value) in slots.iter_mut().zip(args) {
            *slot = Some(value);
        }
        return Ok(slots);
    };
    let mut remaining = kwargs.len();
    if nargs + remaining > 3 {
        let keyword = if nargs == 0 { "keyword " } else { "" };
        return Err(error(
            "TypeError",
            format!("str() takes at most 3 {keyword}arguments ({} given)", nargs + remaining),
        ));
    }
    for (slot, value) in slots.iter_mut().zip(args) {
        *slot = Some(value);
    }
    for index in nargs..3 {
        if remaining == 0 {
            break;
        }
        if let Some(value) = kwargs.get(KEYWORDS[index]) {
            slots[index] = Some(value);
            remaining -= 1;
        }
    }
    if remaining == 0 {
        return Ok(slots);
    }
    for (index, keyword) in KEYWORDS.iter().enumerate().take(nargs) {
        if kwargs.get(keyword).is_some() {
            return Err(error(
                "TypeError",
                format!("argument for str() given by name ('{keyword}') and position ({})", index + 1),
            ));
        }
    }
    Err(unexpected_keyword(kwargs))
}

/// The unexpected-keyword error: the first key in dict order that is not a string, or a string that is no
/// parameter name (with the closest parameter name as a suggestion).
fn unexpected_keyword(kwargs: &Dict) -> NativeException {
    unexpected_keyword_for(kwargs, &KEYWORDS, "str")
}

/// Shared unexpected-keyword error, including suggestions
/// and non-scalar Unicode names, for the other admitted builtin constructors.
pub(super) fn unexpected_keyword_for(kwargs: &Dict, names: &[&str], function: &str) -> NativeException {
    for (key, _) in kwargs.iter() {
        let name: Vec<u32> = match key {
            Key::Str(text) => text.chars().map(u32::from).collect(),
            Key::Extra(NativeKey::Unicode(text)) => text.codepoints().to_vec(),
            _ => return error("TypeError", "keywords must be strings"),
        };
        let scalar: Option<String> = name.iter().map(|&c| char::from_u32(c)).collect();
        if scalar.as_deref().is_some_and(|name| names.contains(&name)) {
            continue;
        }
        // A name with lone surrogates has no UTF-8 form, so it gets no suggestion; the message keeps them.
        let suggestion = scalar.as_deref().and_then(|name| suggest(name.as_bytes(), names));
        let mut message: Vec<u32> =
            format!("{function}() got an unexpected keyword argument '").chars().map(u32::from).collect();
        message.extend(&name);
        let tail = match suggestion {
            Some(keyword) => format!("'. Did you mean '{keyword}'?"),
            None => "'".into(),
        };
        message.extend(tail.chars().map(u32::from));
        return NativeException::new("TypeError", PythonString::from_codepoints(message).expect("code points"));
    }
    error("TypeError", format!("invalid keyword argument for {function}()"))
}

const MOVE_COST: usize = 2;

/// The parameter name (`object`, `encoding`, `errors`) closest to `name`, if close enough.
fn suggest<'a>(name: &[u8], names: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(&'a str, usize)> = None;
    for &item in names {
        let mut max_distance = (name.len() + item.len() + 3) * MOVE_COST / 6;
        if let Some((_, distance)) = best {
            max_distance = max_distance.min(distance.saturating_sub(1));
            if distance == 0 {
                continue;
            }
        }
        let distance = levenshtein(name, item.as_bytes(), max_distance);
        if distance > max_distance {
            continue;
        }
        if best.is_none_or(|(_, current)| distance < current) {
            best = Some((item, distance));
        }
    }
    best.map(|(item, _)| item)
}

fn substitution_cost(a: u8, b: u8) -> usize {
    if a & 31 != b & 31 {
        return MOVE_COST;
    }
    if a == b {
        return 0;
    }
    let lower = |c: u8| if c.is_ascii_uppercase() { c + 32 } else { c };
    if lower(a) == lower(b) {
        return 1;
    }
    MOVE_COST
}

/// Weighted edit distance between two names, used to rank suggestions.
fn levenshtein<'a>(mut a: &'a [u8], mut b: &'a [u8], max_cost: usize) -> usize {
    while let (Some(x), Some(y)) = (a.first(), b.first()) {
        if x != y {
            break;
        }
        a = &a[1..];
        b = &b[1..];
    }
    while let (Some(x), Some(y)) = (a.last(), b.last()) {
        if x != y {
            break;
        }
        a = &a[..a.len() - 1];
        b = &b[..b.len() - 1];
    }
    if a.is_empty() || b.is_empty() {
        return (a.len() + b.len()) * MOVE_COST;
    }
    if a.len() > 40 || b.len() > 40 {
        return max_cost + 1;
    }
    if b.len() < a.len() {
        std::mem::swap(&mut a, &mut b);
    }
    if (b.len() - a.len()) * MOVE_COST > max_cost {
        return max_cost + 1;
    }
    let mut buffer: Vec<usize> = (1..=a.len()).map(|i| i * MOVE_COST).collect();
    let mut result = 0;
    for (b_index, &code) in b.iter().enumerate() {
        let mut distance = b_index * MOVE_COST;
        result = distance;
        let mut minimum = usize::MAX;
        for (index, &item) in a.iter().enumerate() {
            let substitute = distance + substitution_cost(code, item);
            distance = buffer[index];
            let insert_delete = result.min(distance) + MOVE_COST;
            result = insert_delete.min(substitute);
            buffer[index] = result;
            minimum = minimum.min(result);
        }
        if minimum > max_cost {
            return max_cost + 1;
        }
    }
    result
}

/// The `encoding` / `errors` argument check: a `str`, UTF-8 encodable, without NUL.
fn text_argument(value: &LegacyValue, name: &str) -> Result<String, NativeException> {
    let resolved = value.resolved();
    let Some(text) =
        resolved.python_string().filter(|_| matches!(resolved, LegacyValue::Str(_) | LegacyValue::Unicode(_)))
    else {
        let shown = if matches!(resolved, LegacyValue::None) { "None" } else { tp_name(&resolved) };
        return Err(error("TypeError", format!("str() argument '{name}' must be str, not {shown}")));
    };
    let text = utf8_strict(&text)?;
    if text.contains('\0') {
        return Err(error("ValueError", "embedded null character"));
    }
    Ok(text)
}

/// `UnicodeDecodeError(encoding, object, start, end, reason)`.
pub(super) fn decode_error(encoding: &str, object: &[u8], start: i64, end: i64, reason: &str) -> NativeException {
    NativeException::unicode_decode(encoding, object, start, end, reason)
}

/// A `%.400s` argument: at most 400 bytes, decoded with replacement.
fn truncated(text: &str) -> String {
    let bytes = text.as_bytes();
    String::from_utf8_lossy(&bytes[..bytes.len().min(400)]).into_owned()
}

/// The `errors` argument a decoder hands to its error handler: `None` is `strict`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Errors<'a>(pub(super) Option<&'a str>);

impl Errors<'_> {
    /// Calls the handler for `UnicodeDecodeError(encoding, object, start, end, reason)`: the replacement and
    /// the position decoding resumes at.
    pub(super) fn handle(
        &self,
        encoding: &str,
        object: &[u8],
        start: usize,
        end: usize,
        reason: &str,
    ) -> Result<(Vec<u32>, usize), NativeException> {
        let raise = || decode_error(encoding, object, start as i64, end as i64, reason);
        // Start and end are clamped to the object.
        let size = object.len();
        let start = start.min(size.saturating_sub(1));
        let end = end.clamp(1, size.max(1));
        match self.0.unwrap_or("strict") {
            "strict" => Err(raise()),
            "ignore" => Ok((Vec::new(), end)),
            "replace" => Ok((vec![0xFFFD], end)),
            "backslashreplace" => {
                let mut out = Vec::new();
                for byte in &object[start..end.max(start)] {
                    out.extend(format!("\\x{byte:02x}").chars().map(u32::from));
                }
                Ok((out, end))
            }
            "surrogateescape" => {
                let escaped: Vec<u32> = object[start..end.max(start)]
                    .iter()
                    .take(4)
                    .take_while(|byte| **byte >= 128)
                    .map(|byte| 0xDC00 + u32::from(*byte))
                    .collect();
                if escaped.is_empty() {
                    return Err(raise());
                }
                let consumed = escaped.len();
                Ok((escaped, start + consumed))
            }
            "surrogatepass" => {
                let Some((width, unit)) = standard_encoding(encoding) else {
                    return Err(raise());
                };
                let p = &object[start..];
                let mut ch = 0;
                if p.len() >= width {
                    ch = match unit {
                        Unit::Utf8 => {
                            if p[0] & 0xF0 == 0xE0 && p[1] & 0xC0 == 0x80 && p[2] & 0xC0 == 0x80 {
                                (u32::from(p[0] & 0x0F) << 12) + (u32::from(p[1] & 0x3F) << 6) + u32::from(p[2] & 0x3F)
                            } else {
                                0
                            }
                        }
                        Unit::Utf16Le => u32::from(u16::from_le_bytes([p[0], p[1]])),
                        Unit::Utf16Be => u32::from(u16::from_be_bytes([p[0], p[1]])),
                        Unit::Utf32Le => u32::from_le_bytes([p[0], p[1], p[2], p[3]]),
                        Unit::Utf32Be => u32::from_be_bytes([p[0], p[1], p[2], p[3]]),
                    };
                }
                if !(0xD800..0xE000).contains(&ch) {
                    return Err(raise());
                }
                Ok((vec![ch], start + width))
            }
            "xmlcharrefreplace" | "namereplace" => {
                Err(error("TypeError", "don't know how to handle UnicodeDecodeError in error callback"))
            }
            name => Err(error("LookupError", format!("unknown error handler name '{}'", truncated(name)))),
        }
    }
}

#[derive(Clone, Copy)]
enum Unit {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

/// The standard UTF encoding a name denotes, if any (native byte order is little-endian).
fn standard_encoding(encoding: &str) -> Option<(usize, Unit)> {
    let bytes = encoding.as_bytes();
    if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"utf") {
        let mut rest = &bytes[3..];
        if let Some(b'-' | b'_') = rest.first() {
            rest = &rest[1..];
        }
        if rest == b"8" {
            return Some((3, Unit::Utf8));
        }
        for (digits, width, le, be) in
            [(b"16", 2, Unit::Utf16Le, Unit::Utf16Be), (b"32", 4, Unit::Utf32Le, Unit::Utf32Be)]
        {
            if rest.starts_with(digits) {
                let mut rest = &rest[2..];
                if rest.is_empty() {
                    return Some((width, le));
                }
                if let Some(b'-' | b'_') = rest.first() {
                    rest = &rest[1..];
                }
                if rest.len() == 2 && rest[1].eq_ignore_ascii_case(&b'e') {
                    match rest[0].to_ascii_lowercase() {
                        b'b' => return Some((width, be)),
                        b'l' => return Some((width, le)),
                        _ => {}
                    }
                }
                return None;
            }
        }
        return None;
    }
    (encoding == "CP_UTF8").then_some((3, Unit::Utf8))
}

/// The normalized encoding name, at most `limit` characters (`None` when it does not fit).
fn normalize(encoding: &str, limit: usize) -> Option<String> {
    let mut out = String::new();
    let mut punct = false;
    for byte in encoding.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'.' {
            if punct && !out.is_empty() {
                out.push('_');
            }
            punct = false;
            out.push(char::from(byte.to_ascii_lowercase()));
            if out.len() > limit {
                return None;
            }
        } else {
            punct = true;
        }
    }
    Some(out)
}

/// Decodes non-empty `data` with the named encoding and error handler.
fn decode(data: &[u8], encoding: Option<&str>, errors: Option<&str>) -> Result<Vec<u32>, NativeException> {
    let handler = Errors(errors);
    let Some(encoding) = encoding else {
        return text::utf8(data, handler);
    };
    if let Some(lower) = normalize(encoding, 10) {
        if let Some(rest) = lower.strip_prefix("utf") {
            let rest = rest.strip_prefix('_').unwrap_or(rest);
            match rest {
                "8" => return text::utf8(data, handler),
                "16" => return text::utf16(data, handler, 0),
                "32" => return text::utf32(data, handler, 0),
                _ => {}
            }
        } else {
            match lower.as_str() {
                "ascii" | "us_ascii" => return text::ascii(data, handler),
                "mbcs" => return text::code_page(data, "mbcs"),
                "latin1" | "latin_1" | "iso_8859_1" | "iso8859_1" => return Ok(text::latin1(data)),
                _ => {}
            }
        }
    }
    let module = match pycodecs::lookup(encoding) {
        Lookup::Found { module, text: true } => module,
        Lookup::Found { text: false, .. } => {
            return Err(error(
                "LookupError",
                format!(
                    "'{}' is not a text encoding; use codecs.decode() to handle arbitrary codecs",
                    truncated(encoding)
                ),
            ));
        }
        Lookup::Unknown => return Err(error("LookupError", format!("unknown encoding: {encoding}"))),
    };
    registry_decode(module, data, errors).map_err(|exception| with_codec_note(exception, encoding))
}

/// Adds the note "decoding with '<encoding>' codec failed" to
/// whatever the registry codec reported.
fn with_codec_note(exception: NativeException, encoding: &str) -> NativeException {
    exception.with_note(PythonString::from(format!("decoding with '{encoding}' codec failed").as_str()))
}

/// The decoder of a codec module, called with the data and, when given, `errors`.
fn registry_decode(module: &str, data: &[u8], errors: Option<&str>) -> Result<Vec<u32>, NativeException> {
    let handler = Errors(errors);
    match module {
        "utf_8" => text::utf8(data, handler),
        "utf_8_sig" => match data.strip_prefix(b"\xEF\xBB\xBF") {
            Some(rest) => text::utf8(rest, handler),
            None => text::utf8(data, handler),
        },
        "utf_16" => text::utf16(data, handler, 0),
        "utf_16_le" => text::utf16(data, handler, -1),
        "utf_16_be" => text::utf16(data, handler, 1),
        "utf_32" => text::utf32(data, handler, 0),
        "utf_32_le" => text::utf32(data, handler, -1),
        "utf_32_be" => text::utf32(data, handler, 1),
        "utf_7" => text::utf7(data, handler),
        "ascii" => text::ascii(data, handler),
        "latin_1" | "charmap" => Ok(text::latin1(data)),
        "mbcs" | "oem" => text::code_page(data, module),
        "unicode_escape" => escape::unicode_escape(data, handler),
        "raw_unicode_escape" => escape::raw_unicode_escape(data, handler),
        "undefined" => Err(error("UnicodeError", "undefined encoding")),
        "idna" => idna::idna(data, errors),
        "punycode" => idna::punycode(data, errors),
        _ => match cjk::Codec::named(module) {
            Some(codec) => cjk::decode(codec, data, handler),
            None => text::charmap(data, module, handler),
        },
    }
}

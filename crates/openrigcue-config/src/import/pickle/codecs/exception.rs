//! The full value of an error the `str` constructor and its codecs report: the
//! class, the message as code points (lone surrogates included), notes, the `UnicodeError` attributes
//! (`encoding`, `object`, `start`, `end`, `reason`) and the error it was raised in the context of.
//!
//! The message ([`NativeException::message`]) excludes the notes; the one-error report
//! ([`NativeException::format_exception_only`]) includes them. Text stays
//! code points until a caller's real output boundary: [`utf8_strict`] reports the `UnicodeEncodeError` of a
//! strict UTF-8 destination, and `Display` renders lone surrogates as backslash escapes.

use super::super::PythonString;
use std::fmt;

/// The `object` attribute of a `UnicodeError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnicodeErrorObject {
    /// `UnicodeDecodeError.object`.
    Bytes(Vec<u8>),
    /// `UnicodeEncodeError.object`.
    Text(PythonString),
}

/// The attributes of a `UnicodeDecodeError` / `UnicodeEncodeError` (`start` and `end` as stored, unclamped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnicodeErrorAttributes {
    pub encoding: PythonString,
    pub object: UnicodeErrorObject,
    pub start: i64,
    pub end: i64,
    pub reason: PythonString,
}

/// A builtin exception value (one pointer wide).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeException(Box<Payload>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Payload {
    class: &'static str,
    message: PythonString,
    notes: Vec<PythonString>,
    unicode: Option<UnicodeErrorAttributes>,
    context: Option<NativeException>,
    suppress_context: bool,
}

fn text(value: &str) -> PythonString {
    PythonString::from(value)
}

impl NativeException {
    /// An exception of `class` whose message is `message`.
    pub fn new(class: &'static str, message: PythonString) -> Self {
        NativeException(Box::new(Payload {
            class,
            message,
            notes: Vec::new(),
            unicode: None,
            context: None,
            suppress_context: false,
        }))
    }

    pub(crate) fn plain(class: &'static str, message: impl AsRef<str>) -> Self {
        Self::new(class, text(message.as_ref()))
    }

    /// A `UnicodeDecodeError` with its attributes and formatted message.
    pub(crate) fn unicode_decode(encoding: &str, object: &[u8], start: i64, end: i64, reason: &str) -> Self {
        let len = object.len() as i64;
        let message = if (0..len).contains(&start) && (0..=len).contains(&end) && end == start + 1 {
            format!(
                "'{encoding}' codec can't decode byte 0x{:02x} in position {start}: {reason}",
                object[start as usize]
            )
        } else {
            format!("'{encoding}' codec can't decode bytes in position {start}-{}: {reason}", end - 1)
        };
        let mut exception = Self::plain("UnicodeDecodeError", message);
        exception.0.unicode = Some(UnicodeErrorAttributes {
            encoding: text(encoding),
            object: UnicodeErrorObject::Bytes(object.to_vec()),
            start,
            end,
            reason: text(reason),
        });
        exception
    }

    /// A `UnicodeEncodeError` with its attributes and formatted message.
    pub(crate) fn unicode_encode(encoding: &str, object: &[u32], start: i64, end: i64, reason: &str) -> Self {
        let len = object.len() as i64;
        let message = if (0..len).contains(&start) && (0..=len).contains(&end) && end == start + 1 {
            let c = object[start as usize];
            let shown = match c {
                0..=0xFF => format!("\\x{c:02x}"),
                0x100..=0xFFFF => format!("\\u{c:04x}"),
                _ => format!("\\U{c:08x}"),
            };
            format!("'{encoding}' codec can't encode character '{shown}' in position {start}: {reason}")
        } else {
            format!("'{encoding}' codec can't encode characters in position {start}-{}: {reason}", end - 1)
        };
        let mut exception = Self::plain("UnicodeEncodeError", message);
        exception.0.unicode = Some(UnicodeErrorAttributes {
            encoding: text(encoding),
            object: UnicodeErrorObject::Text(PythonString::from_codepoints(object.to_vec()).expect("code points")),
            start,
            end,
            reason: text(reason),
        });
        exception
    }

    /// Appends a note.
    pub(crate) fn with_note(mut self, note: PythonString) -> Self {
        self.0.notes.push(note);
        self
    }

    /// Records that this was raised while handling `context` (hidden from reports when `suppress`).
    pub(crate) fn with_context(mut self, context: NativeException, suppress: bool) -> Self {
        self.0.context = Some(context);
        self.0.suppress_context = suppress;
        self
    }

    /// The builtin class name (without a module prefix).
    pub fn class(&self) -> &'static str {
        self.0.class
    }

    /// The message; never includes the notes.
    pub fn message(&self) -> &PythonString {
        &self.0.message
    }

    /// The notes (empty when the exception has none).
    pub fn notes(&self) -> &[PythonString] {
        &self.0.notes
    }

    /// The `UnicodeDecodeError` / `UnicodeEncodeError` attributes.
    pub fn unicode_error(&self) -> Option<&UnicodeErrorAttributes> {
        self.0.unicode.as_ref()
    }

    /// The exception being handled when this one was raised.
    pub fn context(&self) -> Option<&NativeException> {
        self.0.context.as_ref()
    }

    /// Whether the context is hidden from reports.
    pub fn suppress_context(&self) -> bool {
        self.0.suppress_context
    }

    /// The one-error report: `Class: message` (or `Class` for an empty message), then
    /// every line of every note, each line ending in a newline.
    pub fn format_exception_only(&self) -> PythonString {
        let mut out: Vec<u32> = self.0.class.chars().map(u32::from).collect();
        if !self.0.message.is_empty() {
            out.extend([u32::from(':'), u32::from(' ')]);
            out.extend_from_slice(self.0.message.codepoints());
        }
        out.push(u32::from('\n'));
        for note in &self.0.notes {
            for line in note.codepoints().split(|c| *c == u32::from('\n')) {
                out.extend_from_slice(line);
                out.push(u32::from('\n'));
            }
        }
        PythonString::from_codepoints(out).expect("code points")
    }
}

/// Strict UTF-8 encoding of `text`: the text, or its `UnicodeEncodeError` (the first run
/// of surrogates, "surrogates not allowed").
pub fn utf8_strict(text: &PythonString) -> Result<String, NativeException> {
    let points = text.codepoints();
    let surrogate = |c: &u32| (0xD800..0xE000).contains(c);
    match points.iter().position(surrogate) {
        Some(start) => {
            let end = start + points[start..].iter().take_while(|c| surrogate(c)).count();
            Err(NativeException::unicode_encode("utf-8", points, start as i64, end as i64, "surrogates not allowed"))
        }
        None => Ok(text.scalar_string().expect("no surrogates")),
    }
}

/// Text for an error stream (UTF-8 with backslash escapes): lone surrogates as `\udXXX`.
pub fn backslashreplace(text: &PythonString) -> String {
    text.codepoints().iter().map(|&c| char::from_u32(c).map_or_else(|| format!("\\u{c:04x}"), String::from)).collect()
}

/// `Class: message` for an error stream (the notes are not part of the message).
impl fmt::Display for NativeException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.class)?;
        if !self.0.message.is_empty() {
            write!(f, ": {}", backslashreplace(&self.0.message))?;
        }
        Ok(())
    }
}

impl std::error::Error for NativeException {}

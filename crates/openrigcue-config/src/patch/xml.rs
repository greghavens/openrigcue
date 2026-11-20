//! The XML element tree the patch modules use (namespace-aware, with `}` separating namespace and local name):
//! parsing, an indent helper, and serialization.
//!
//! Parsing: encoding from the byte-order mark or the XML declaration (UTF-8, UTF-16,
//! ISO-8859-1 and US-ASCII directly; any other name goes through a single-byte map, [`pycodecs::expat_byte_map`]),
//! XML 1.0 characters and name tables, line ends and attribute whitespace normalized, namespaces
//! resolved to `{uri}local` names (with the reserved-prefix rules), the internal DTD subset
//! (internal general entities, also with markup; attribute defaults and tokenized-type normalization from
//! `ATTLIST`; parameter entities left unread, after which later
//! declarations are skipped unless the document is standalone), comments and processing instructions dropped.

use std::collections::HashMap;

use super::pycodecs;
use super::text::is_py_space;
use super::xml_names::{NAME_CHAR, NAME_START};

/// An element with a text/tail model. A name in a namespace is written `{uri}local`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    pub tag: String,
    /// Attributes in document (or insertion) order, defaulted ones after the specified ones.
    pub attributes: Vec<(String, String)>,
    /// Character data before the first child.
    pub text: Option<String>,
    /// Character data after the end tag, up to the next sibling or the parent's end tag.
    pub tail: Option<String>,
    pub children: Vec<Element>,
}

/// Why a document could not be parsed (a syntax error, or an unsupported
/// multi-byte encoding).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum XmlError {
    #[error("malformed XML at character {offset}: {reason}")]
    Syntax { offset: usize, reason: &'static str },
    #[error("unsupported XML encoding {0:?}")]
    Encoding(String),
}

impl Element {
    /// A new element with the given attributes, no text and no children.
    pub fn new(tag: &str, attributes: Vec<(String, String)>) -> Self {
        Element { tag: tag.to_owned(), attributes, ..Element::default() }
    }

    /// Attribute value.
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// Sets an attribute in place, or appends it.
    pub fn set_attribute(&mut self, name: &str, value: &str) {
        match self.attributes.iter_mut().find(|(key, _)| key == name) {
            Some((_, existing)) => *existing = value.to_owned(),
            None => self.attributes.push((name.to_owned(), value.to_owned())),
        }
    }

    /// Path (child indices) of the first descendant with this tag in document order (`find('.//tag')`).
    pub fn find_descendant_path(&self, tag: &str) -> Option<Vec<usize>> {
        for (index, child) in self.children.iter().enumerate() {
            if child.tag == tag {
                return Some(vec![index]);
            }
            if let Some(mut path) = child.find_descendant_path(tag) {
                path.insert(0, index);
                return Some(path);
            }
        }
        None
    }

    /// The descendant at a child-index path.
    pub fn at_path_mut(&mut self, path: &[usize]) -> &mut Element {
        path.iter().fold(self, |element, index| &mut element.children[*index])
    }
}

const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";
/// The namespace/local-name separator; a namespace name containing it is rejected.
const NAMESPACE_SEPARATOR: char = '}';
/// Entity expansion nesting beyond which parsing fails instead of exhausting the stack.
const MAX_ENTITY_DEPTH: usize = 512;
/// Entity-expansion ("billion laughs") protection: amplification above this factor fails once the output
/// passes the activation threshold.
const AMPLIFICATION_FACTOR: f64 = 100.0;
const AMPLIFICATION_THRESHOLD: usize = 8 * 1024 * 1024;

fn syntax<T>(offset: usize, reason: &'static str) -> Result<T, XmlError> {
    Err(XmlError::Syntax { offset, reason })
}

/// Parses a file's bytes.
pub fn parse_bytes(bytes: &[u8]) -> Result<Element, XmlError> {
    parse_chars(decode_document(bytes, false)?)
}

/// Parses a document held as text: its UTF-8 bytes are parsed with the protocol
/// encoding set to UTF-8, so the declaration's encoding is not used (start-of-input detection still
/// applies: a byte-order mark is skipped, and text starting `"<\0"` or `"\0<"` is read as UTF-16).
pub fn parse_str(document: &str) -> Result<Element, XmlError> {
    parse_chars(decode_document(document.as_bytes(), true)?)
}

fn parse_chars(chars: Vec<char>) -> Result<Element, XmlError> {
    let normalized = normalize_newlines(chars);
    let mut parser = Parser::new(&normalized);
    parser.document(&normalized)
}

/// `\r\n` and lone `\r` become `\n` (character references are not affected).
fn normalize_newlines(chars: Vec<char>) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut iter = chars.into_iter().peekable();
    while let Some(c) = iter.next() {
        if c == '\r' {
            if iter.peek() == Some(&'\n') {
                iter.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------
// Encoding detection and decoding.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Utf16 {
    Little,
    Big,
}

/// Decodes a document: the byte-order mark or the first bytes give UTF-16 (either order) or UTF-8; without a
/// protocol encoding the XML declaration's encoding then applies.
fn decode_document(bytes: &[u8], protocol_utf8: bool) -> Result<Vec<char>, XmlError> {
    let (utf16, body) = match bytes {
        [0xFE, 0xFF, rest @ ..] => (Some(Utf16::Big), rest),
        [0xFF, 0xFE, rest @ ..] => (Some(Utf16::Little), rest),
        [0xEF, 0xBB, 0xBF, rest @ ..] => (None, rest),
        [0x00, 0x3C, ..] => (Some(Utf16::Big), bytes),
        [0x3C, 0x00, ..] => (Some(Utf16::Little), bytes),
        _ => (None, bytes),
    };
    if let Some(order) = utf16 {
        let chars = decode_utf16(body, order)?;
        if protocol_utf8 {
            return Ok(chars);
        }
        let declared = declared_encoding(&chars)?;
        return match declared.as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("utf-16") => Ok(chars),
            Some("utf-16le") if order == Utf16::Little => Ok(chars),
            Some("utf-16be") if order == Utf16::Big => Ok(chars),
            _ => Err(XmlError::Encoding(declared.unwrap_or_default())),
        };
    }
    // The declaration is ASCII in every encoding that can follow an 8-bit start.
    let head: Vec<char> = body.iter().take_while(|b| **b != b'>').map(|b| char::from(*b)).chain(['>']).collect();
    let declared = declared_encoding(&head)?;
    let Some(name) = declared.filter(|_| !protocol_utf8) else {
        return decode_utf8(body);
    };
    match name.to_ascii_lowercase().as_str() {
        "utf-8" => decode_utf8(body),
        "iso-8859-1" => Ok(body.iter().map(|b| char::from(*b)).collect()),
        "us-ascii" => body
            .iter()
            .enumerate()
            .map(
                |(index, b)| {
                    if b.is_ascii() {
                        Ok(char::from(*b))
                    } else {
                        syntax(index, "not well-formed (invalid token)")
                    }
                },
            )
            .collect(),
        "utf-16" | "utf-16le" | "utf-16be" => Err(XmlError::Encoding(name)),
        _ => {
            let map = pycodecs::expat_byte_map(&name).ok_or_else(|| XmlError::Encoding(name.clone()))?;
            body.iter()
                .enumerate()
                .map(|(index, b)| map[*b as usize].map_or_else(|| syntax(index, "not well-formed (invalid token)"), Ok))
                .collect()
        }
    }
}

fn decode_utf8(bytes: &[u8]) -> Result<Vec<char>, XmlError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.chars().collect()),
        Err(error) => syntax(error.valid_up_to(), "not well-formed (invalid token)"),
    }
}

fn decode_utf16(bytes: &[u8], order: Utf16) -> Result<Vec<char>, XmlError> {
    if !bytes.len().is_multiple_of(2) {
        return syntax(bytes.len() / 2, "unclosed token");
    }
    let units = bytes.as_chunks::<2>().0.iter().map(|pair| match order {
        Utf16::Little => u16::from_le_bytes([pair[0], pair[1]]),
        Utf16::Big => u16::from_be_bytes([pair[0], pair[1]]),
    });
    char::decode_utf16(units)
        .enumerate()
        .map(|(index, unit)| unit.or_else(|_| syntax(index, "not well-formed (invalid token)")))
        .collect()
}

/// The `encoding` of an XML declaration at the very start, after checking the declaration's syntax.
fn declared_encoding(chars: &[char]) -> Result<Option<String>, XmlError> {
    if !starts_declaration(chars) {
        return Ok(None);
    }
    Ok(parse_declaration(chars)?.encoding)
}

fn starts_declaration(chars: &[char]) -> bool {
    let prefix: Vec<char> = "<?xml".chars().collect();
    chars.starts_with(&prefix) && matches!(chars.get(5), Some(' ' | '\t' | '\n' | '\r') | Some('?'))
}

struct Declaration {
    encoding: Option<String>,
    standalone: bool,
    end: usize,
}

/// The XML declaration: `version` first, then optionally `encoding` (an EncName), then optionally
/// `standalone` (`yes`/`no`); every pseudo-attribute value is made of ASCII letters, digits, `.`, `_`, `-`.
fn parse_declaration(chars: &[char]) -> Result<Declaration, XmlError> {
    let at = |index: usize| chars.get(index).copied();
    let is_space = |c: Option<char>| matches!(c, Some(' ' | '\t' | '\n' | '\r'));
    let mut index = 5;
    let mut attributes: Vec<(String, String)> = Vec::new();
    loop {
        let had_space = is_space(at(index));
        while is_space(at(index)) {
            index += 1;
        }
        if at(index) == Some('?') && at(index + 1) == Some('>') {
            index += 2;
            break;
        }
        if !had_space {
            return syntax(index, "XML declaration not well-formed");
        }
        let name_start = index;
        while at(index).is_some_and(|c| c.is_ascii_alphabetic()) {
            index += 1;
        }
        let name: String = chars[name_start..index].iter().collect();
        while is_space(at(index)) {
            index += 1;
        }
        if name.is_empty() || at(index) != Some('=') {
            return syntax(index, "XML declaration not well-formed");
        }
        index += 1;
        while is_space(at(index)) {
            index += 1;
        }
        let Some(quote) = at(index).filter(|c| *c == '"' || *c == '\'') else {
            return syntax(index, "XML declaration not well-formed");
        };
        index += 1;
        let value_start = index;
        loop {
            match at(index) {
                Some(c) if c == quote => break,
                Some(c) if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') => index += 1,
                _ => return syntax(index, "XML declaration not well-formed"),
            }
        }
        let value: String = chars[value_start..index].iter().collect();
        index += 1;
        attributes.push((name, value));
    }
    let mut rest = attributes.into_iter().peekable();
    if rest.next_if(|(name, _)| name == "version").is_none() {
        return syntax(5, "XML declaration not well-formed");
    }
    let encoding = rest.next_if(|(name, _)| name == "encoding").map(|(_, value)| value);
    if encoding.as_ref().is_some_and(|name| !name.starts_with(|c: char| c.is_ascii_alphabetic())) {
        return syntax(5, "XML declaration not well-formed");
    }
    let standalone = match rest.next_if(|(name, _)| name == "standalone") {
        None => false,
        Some((_, value)) if value == "yes" => true,
        Some((_, value)) if value == "no" => false,
        Some(_) => return syntax(5, "XML declaration not well-formed"),
    };
    if rest.next().is_some() {
        return syntax(5, "XML declaration not well-formed");
    }
    Ok(Declaration { encoding, standalone, end: index })
}

// ---------------------------------------------------------------------------------------------------------
// Characters and names.

/// XML 1.0 `Char`.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

fn in_ranges(ranges: &[(u32, u32)], c: char) -> bool {
    let code = u32::from(c);
    let index = ranges.partition_point(|(first, _)| *first <= code);
    index > 0 && code <= ranges[index - 1].1
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || (!c.is_ascii() && in_ranges(&NAME_START, c))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') || (!c.is_ascii() && in_ranges(&NAME_CHAR, c))
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

// ---------------------------------------------------------------------------------------------------------
// The DTD.

#[derive(Debug, Clone)]
enum EntityKind {
    /// Replacement text (character references already expanded, entity references kept).
    Internal(Vec<char>),
    External,
    Unparsed,
}

/// A declared attribute of an element.
#[derive(Debug, Clone)]
struct DeclaredAttribute {
    name: String,
    default: Option<String>,
    is_cdata: bool,
}

#[derive(Debug, Default)]
struct Dtd {
    entities: HashMap<String, EntityKind>,
    /// Element name → declared attributes in declaration order.
    attributes: HashMap<String, Vec<DeclaredAttribute>>,
    standalone: bool,
    has_param_entity_refs: bool,
    keep_processing: bool,
}

impl Dtd {
    /// Whether a reference to an undeclared entity is an error.
    fn check_entity_declarations(&self) -> bool {
        !self.has_param_entity_refs || self.standalone
    }

    fn declared(&self, element: &str, attribute: &str) -> Option<&DeclaredAttribute> {
        self.attributes.get(element)?.iter().find(|declared| declared.name == attribute)
    }
}

// ---------------------------------------------------------------------------------------------------------
// The tree builder.

/// A namespace binding: prefix (None = the default namespace) → namespace name (None = no namespace).
type Binding = (Option<String>, Option<String>);

#[derive(Default)]
struct Builder {
    stack: Vec<Element>,
    /// Raw qualified names of the open elements and the namespace bindings each one added.
    open: Vec<(String, Vec<Binding>)>,
    data: String,
    root: Option<Element>,
}

impl Builder {
    fn flush(&mut self) {
        if self.data.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.data);
        if let Some(parent) = self.stack.last_mut() {
            let slot = match parent.children.last_mut() {
                Some(child) => &mut child.tail,
                None => &mut parent.text,
            };
            slot.get_or_insert_with(String::new).push_str(&data);
        }
    }

    fn start(&mut self, element: Element) {
        self.flush();
        self.stack.push(element);
    }

    fn end(&mut self) {
        self.flush();
        if let Some(element) = self.stack.pop() {
            match self.stack.last_mut() {
                Some(parent) => parent.children.push(element),
                None => self.root = Some(element),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The parser.

struct Parser {
    dtd: Dtd,
    builder: Builder,
    /// Prefix (None = default namespace) → namespace name (None = no namespace), innermost last.
    bindings: Vec<Binding>,
    open_entities: Vec<String>,
    direct: usize,
    indirect: usize,
}

/// A run of characters being parsed: the document or an entity's replacement text.
type Source<'a> = &'a [char];

impl Parser {
    fn new(chars: &[char]) -> Self {
        Parser {
            dtd: Dtd { keep_processing: true, ..Dtd::default() },
            builder: Builder::default(),
            bindings: vec![(Some("xml".to_owned()), Some(XML_NAMESPACE.to_owned()))],
            open_entities: Vec::new(),
            direct: chars.len(),
            indirect: 0,
        }
    }

    fn document(&mut self, s: Source<'_>) -> Result<Element, XmlError> {
        let mut index = 0;
        if starts_declaration(s) {
            // The encoding was applied while decoding (and is not used for text).
            let declaration = parse_declaration(s)?;
            self.dtd.standalone = declaration.standalone;
            index = declaration.end;
        }
        let mut seen_doctype = false;
        loop {
            index = skip_spaces(s, index);
            match s.get(index) {
                None => return syntax(index, "no element found"),
                Some('<') => {}
                Some(_) => return syntax(index, "syntax error"),
            }
            if starts_with(s, index, "<!--") {
                index = comment(s, index)?;
            } else if starts_with(s, index, "<?") {
                index = processing_instruction(s, index)?;
            } else if starts_with(s, index, "<!DOCTYPE") && !seen_doctype {
                seen_doctype = true;
                index = self.doctype(s, index)?;
            } else if s.get(index + 1).copied().is_some_and(is_name_start) {
                break;
            } else {
                return syntax(index, "syntax error");
            }
        }
        index = self.content(s, index, true)?;
        loop {
            index = skip_spaces(s, index);
            if index >= s.len() {
                break;
            }
            if starts_with(s, index, "<!--") {
                index = comment(s, index)?;
            } else if starts_with(s, index, "<?") {
                index = processing_instruction(s, index)?;
            } else {
                return syntax(index, "junk after document element");
            }
        }
        self.builder.root.take().ok_or(XmlError::Syntax { offset: index, reason: "no element found" })
    }

    // -------------------------------------------------------------------------------------------------
    // Content.

    /// Parses content from `index`: for the document, the root element (returning after its end tag); for an
    /// entity's replacement text, everything up to its end, which must leave the open elements as they were.
    fn content(&mut self, s: Source<'_>, mut index: usize, document: bool) -> Result<usize, XmlError> {
        let base = self.builder.open.len();
        loop {
            if document && self.builder.open.len() == base && self.builder.root.is_some() {
                return Ok(index);
            }
            let Some(&c) = s.get(index) else {
                if document {
                    return syntax(index, "no element found");
                }
                if self.builder.open.len() != base {
                    return syntax(index, "asynchronous entity");
                }
                return Ok(index);
            };
            match c {
                '<' => {
                    if starts_with(s, index, "</") {
                        if self.builder.open.len() == base {
                            return syntax(index, if document { "syntax error" } else { "asynchronous entity" });
                        }
                        index = self.end_tag(s, index)?;
                    } else if starts_with(s, index, "<!--") {
                        index = comment(s, index)?;
                    } else if starts_with(s, index, "<![CDATA[") {
                        index = self.cdata(s, index)?;
                    } else if starts_with(s, index, "<?") {
                        index = processing_instruction(s, index)?;
                    } else if s.get(index + 1).copied().is_some_and(is_name_start) {
                        index = self.start_tag(s, index)?;
                    } else {
                        return syntax(index, "not well-formed (invalid token)");
                    }
                }
                '&' => index = self.content_reference(s, index)?,
                _ => {
                    if self.builder.open.len() == base && document {
                        return syntax(index, "syntax error");
                    }
                    if c == ']' && starts_with(s, index, "]]>") {
                        return syntax(index, "not well-formed (invalid token)");
                    }
                    if !is_xml_char(c) {
                        return syntax(index, "not well-formed (invalid token)");
                    }
                    if self.builder.open.is_empty() {
                        return syntax(index, "syntax error");
                    }
                    self.builder.data.push(c);
                    index += 1;
                }
            }
        }
    }

    fn cdata(&mut self, s: Source<'_>, index: usize) -> Result<usize, XmlError> {
        let start = index + "<![CDATA[".len();
        let mut end = start;
        loop {
            match s.get(end) {
                None => return syntax(index, "unclosed CDATA section"),
                Some(']') if starts_with(s, end, "]]>") => break,
                Some(&c) if is_xml_char(c) => end += 1,
                Some(_) => return syntax(end, "not well-formed (invalid token)"),
            }
        }
        self.builder.data.extend(&s[start..end]);
        Ok(end + 3)
    }

    /// `&#…;`, a predefined entity, or a declared internal entity (expanded as content).
    fn content_reference(&mut self, s: Source<'_>, index: usize) -> Result<usize, XmlError> {
        if s.get(index + 1) == Some(&'#') {
            let (c, end) = char_reference(s, index)?;
            self.builder.data.push(c);
            return Ok(end);
        }
        let (name, end) = entity_reference_name(s, index)?;
        if let Some(c) = predefined(&name) {
            self.builder.data.push(c);
            return Ok(end);
        }
        let entity = self.dtd.entities.get(&name).cloned();
        let text = match entity {
            Some(EntityKind::Internal(text)) => text,
            Some(EntityKind::Unparsed) => return syntax(index, "reference to binary entity"),
            // The others are an "undefined entity" error.
            Some(EntityKind::External) | None => return syntax(index, "undefined entity"),
        };
        if self.open_entities.contains(&name) {
            return syntax(index, "recursive entity reference");
        }
        if self.open_entities.len() >= MAX_ENTITY_DEPTH {
            return syntax(index, "entity nesting too deep");
        }
        self.account(text.len(), index)?;
        self.open_entities.push(name);
        let result = self.content(&text, 0, false);
        self.open_entities.pop();
        result?;
        Ok(end)
    }

    fn account(&mut self, expanded: usize, index: usize) -> Result<(), XmlError> {
        self.indirect += expanded;
        let total = self.direct + self.indirect;
        if total >= AMPLIFICATION_THRESHOLD && total as f64 / self.direct.max(1) as f64 > AMPLIFICATION_FACTOR {
            return syntax(index, "limit on input amplification factor (from DTD and entities) breached");
        }
        Ok(())
    }

    fn start_tag(&mut self, s: Source<'_>, index: usize) -> Result<usize, XmlError> {
        let (qname, mut pos) = qualified_name(s, index + 1)?;
        let mut specified: Vec<(String, String)> = Vec::new();
        let empty;
        loop {
            let had_space = s.get(pos).copied().is_some_and(is_space);
            pos = skip_spaces(s, pos);
            match s.get(pos) {
                Some('/') if s.get(pos + 1) == Some(&'>') => {
                    empty = true;
                    pos += 2;
                    break;
                }
                Some('>') => {
                    empty = false;
                    pos += 1;
                    break;
                }
                Some(&c) if had_space && is_name_start(c) => {
                    let (name, after) = qualified_name(s, pos)?;
                    pos = skip_spaces(s, after);
                    if s.get(pos) != Some(&'=') {
                        return syntax(pos, "not well-formed (invalid token)");
                    }
                    pos = skip_spaces(s, pos + 1);
                    let is_cdata = self.dtd.declared(&qname, &name).is_none_or(|declared| declared.is_cdata);
                    let (value, after) = self.attribute_value(s, pos, is_cdata)?;
                    if specified.iter().any(|(existing, _)| *existing == name) {
                        return syntax(pos, "duplicate attribute");
                    }
                    specified.push((name, value));
                    pos = after;
                }
                None => return syntax(pos, "unclosed token"),
                Some(_) => return syntax(pos, "not well-formed (invalid token)"),
            }
        }
        // Attribute defaults: the first declaration with a value of each attribute not specified.
        let mut attributes = specified;
        if let Some(declared) = self.dtd.attributes.get(&qname) {
            for attribute in declared {
                if let Some(default) = &attribute.default {
                    if !attributes.iter().any(|(name, _)| *name == attribute.name) {
                        attributes.push((attribute.name.clone(), default.clone()));
                    }
                }
            }
        }
        let (element, added) = self.resolve(&qname, attributes, index)?;
        self.bindings.extend(added.iter().cloned());
        self.builder.open.push((qname, added));
        self.builder.start(element);
        if empty {
            self.close_element();
        }
        Ok(pos)
    }

    fn end_tag(&mut self, s: Source<'_>, index: usize) -> Result<usize, XmlError> {
        let (qname, pos) = qualified_name(s, index + 2)?;
        let pos = skip_spaces(s, pos);
        if s.get(pos) != Some(&'>') {
            return syntax(pos, "not well-formed (invalid token)");
        }
        match self.builder.open.last() {
            Some((open, _)) if *open == qname => {}
            _ => return syntax(index, "mismatched tag"),
        }
        self.close_element();
        Ok(pos + 1)
    }

    fn close_element(&mut self) {
        if let Some((_, scope)) = self.builder.open.pop() {
            self.bindings.truncate(self.bindings.len() - scope.len());
        }
        self.builder.end();
    }

    /// Namespace processing of one start tag: `xmlns` attributes become
    /// bindings, prefixed names are expanded, expanded duplicates rejected.
    fn resolve(
        &self,
        qname: &str,
        attributes: Vec<(String, String)>,
        at: usize,
    ) -> Result<(Element, Vec<Binding>), XmlError> {
        let mut added: Vec<Binding> = Vec::new();
        let mut plain: Vec<(String, String)> = Vec::new();
        for (name, value) in attributes {
            let prefix = if name == "xmlns" {
                None
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                Some(prefix.to_owned())
            } else {
                plain.push((name, value));
                continue;
            };
            check_binding(prefix.as_deref(), &value, at)?;
            added.push((prefix, (!value.is_empty()).then_some(value)));
        }
        let lookup = |prefix: Option<&str>| -> Option<Option<String>> {
            added
                .iter()
                .rev()
                .chain(self.bindings.iter().rev())
                .find(|(bound, _)| bound.as_deref() == prefix)
                .map(|(_, uri)| uri.clone())
        };
        let mut expanded: Vec<(String, String)> = Vec::new();
        for (name, value) in plain {
            let universal = match name.split_once(':') {
                Some((prefix, local)) => match lookup(Some(prefix)) {
                    Some(Some(uri)) => format!("{{{uri}}}{local}"),
                    _ => return syntax(at, "unbound prefix"),
                },
                None => name,
            };
            if expanded.iter().any(|(existing, _)| *existing == universal) {
                return syntax(at, "duplicate attribute");
            }
            expanded.push((universal, value));
        }
        let tag = match qname.split_once(':') {
            Some((prefix, local)) => match lookup(Some(prefix)) {
                Some(Some(uri)) => format!("{{{uri}}}{local}"),
                _ => return syntax(at, "unbound prefix"),
            },
            None => match lookup(None) {
                Some(Some(uri)) => format!("{{{uri}}}{qname}"),
                _ => qname.to_owned(),
            },
        };
        Ok((Element::new(&tag, expanded), added))
    }

    /// An attribute value literal: references expanded, whitespace characters as spaces, collapsed for a
    /// declared non-CDATA attribute.
    fn attribute_value(&mut self, s: Source<'_>, index: usize, is_cdata: bool) -> Result<(String, usize), XmlError> {
        let Some(&quote) = s.get(index).filter(|c| **c == '"' || **c == '\'') else {
            return syntax(index, "not well-formed (invalid token)");
        };
        let mut end = index + 1;
        while s.get(end).is_some_and(|c| *c != quote) {
            end += 1;
        }
        if end >= s.len() {
            return syntax(index, "unclosed token");
        }
        let mut value = String::new();
        self.append_attribute_text(&s[index + 1..end], index + 1, &mut value)?;
        if !is_cdata {
            value = value.split(' ').filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" ");
        }
        Ok((value, end + 1))
    }

    /// Appends the expansion of an attribute value.
    fn append_attribute_text(&mut self, s: Source<'_>, offset: usize, out: &mut String) -> Result<(), XmlError> {
        let mut index = 0;
        while let Some(&c) = s.get(index) {
            match c {
                '<' => return syntax(offset + index, "not well-formed (invalid token)"),
                '&' if s.get(index + 1) == Some(&'#') => {
                    let (c, end) = char_reference(s, index)?;
                    out.push(c);
                    index = end;
                }
                '&' => {
                    let (name, end) = entity_reference_name(s, index)?;
                    index = end;
                    if let Some(c) = predefined(&name) {
                        out.push(c);
                        continue;
                    }
                    let entity = self.dtd.entities.get(&name).cloned();
                    let text = match entity {
                        None if self.dtd.check_entity_declarations() => {
                            return syntax(offset + index, "undefined entity");
                        }
                        // Dropped: a skipped entity inside an attribute value is not reported.
                        None => continue,
                        Some(EntityKind::Unparsed) => return syntax(offset + index, "reference to binary entity"),
                        Some(EntityKind::External) => {
                            return syntax(offset + index, "reference to external entity in attribute");
                        }
                        Some(EntityKind::Internal(text)) => text,
                    };
                    if self.open_entities.contains(&name) {
                        return syntax(offset + index, "recursive entity reference");
                    }
                    if self.open_entities.len() >= MAX_ENTITY_DEPTH {
                        return syntax(offset + index, "entity nesting too deep");
                    }
                    self.account(text.len(), offset + index)?;
                    self.open_entities.push(name);
                    let result = self.append_attribute_text(&text, offset + index, out);
                    self.open_entities.pop();
                    result?;
                }
                '\t' | '\n' | '\r' => {
                    out.push(' ');
                    index += 1;
                }
                c if is_xml_char(c) => {
                    out.push(c);
                    index += 1;
                }
                _ => return syntax(offset + index, "not well-formed (invalid token)"),
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------------------------------------
    // The document type declaration.

    fn doctype(&mut self, s: Source<'_>, index: usize) -> Result<usize, XmlError> {
        let mut pos = index + "<!DOCTYPE".len();
        pos = require_spaces(s, pos)?;
        let (_, after) = prolog_name(s, pos)?;
        pos = after;
        let spaced = s.get(pos).copied().is_some_and(is_space);
        pos = skip_spaces(s, pos);
        if spaced && (starts_with(s, pos, "SYSTEM") || starts_with(s, pos, "PUBLIC")) {
            pos = external_id(s, pos, false)?.0;
            // The external subset counts as a parameter entity reference (never read).
            self.dtd.has_param_entity_refs = true;
            pos = skip_spaces(s, pos);
        }
        if s.get(pos) == Some(&'[') {
            pos = self.internal_subset(s, pos + 1)?;
            pos = skip_spaces(s, pos);
        }
        if s.get(pos) != Some(&'>') {
            return syntax(pos, "syntax error");
        }
        Ok(pos + 1)
    }

    fn internal_subset(&mut self, s: Source<'_>, mut pos: usize) -> Result<usize, XmlError> {
        loop {
            pos = skip_spaces(s, pos);
            match s.get(pos) {
                None => return syntax(pos, "unclosed token"),
                Some(']') => return Ok(pos + 1),
                Some('%') => {
                    let (_, end) = reference_name(s, pos, '%')?;
                    // Parameter entities are never expanded; later declarations only count when the
                    // document is standalone.
                    self.dtd.has_param_entity_refs = true;
                    self.dtd.keep_processing = self.dtd.standalone;
                    pos = end;
                }
                Some('<') if starts_with(s, pos, "<!--") => pos = comment(s, pos)?,
                Some('<') if starts_with(s, pos, "<?") => pos = processing_instruction(s, pos)?,
                Some('<') if starts_with(s, pos, "<!ENTITY") => pos = self.entity_declaration(s, pos)?,
                Some('<') if starts_with(s, pos, "<!ATTLIST") => pos = self.attlist_declaration(s, pos)?,
                Some('<') if starts_with(s, pos, "<!ELEMENT") => pos = element_declaration(s, pos)?,
                Some('<') if starts_with(s, pos, "<!NOTATION") => pos = notation_declaration(s, pos)?,
                Some(_) => return syntax(pos, "syntax error"),
            }
        }
    }

    fn entity_declaration(&mut self, s: Source<'_>, index: usize) -> Result<usize, XmlError> {
        let mut pos = require_spaces(s, index + "<!ENTITY".len())?;
        let parameter = s.get(pos) == Some(&'%');
        if parameter {
            pos = require_spaces(s, pos + 1)?;
        }
        let (name, after) = nc_name(s, pos).or_else(|_| syntax(pos, "syntax error"))?;
        pos = require_spaces(s, after)?;
        let kind = if matches!(s.get(pos), Some('"' | '\'')) {
            let (text, after) = self.entity_value(s, pos)?;
            pos = after;
            EntityKind::Internal(text)
        } else {
            let (after, _) = external_id(s, pos, false)?;
            pos = after;
            let spaced = s.get(pos).copied().is_some_and(is_space);
            let next = skip_spaces(s, pos);
            if spaced && starts_with(s, next, "NDATA") {
                if parameter {
                    return syntax(next, "syntax error");
                }
                let notation = require_spaces(s, next + "NDATA".len())?;
                let (_, after) = nc_name(s, notation).or_else(|_| syntax(notation, "syntax error"))?;
                pos = after;
                EntityKind::Unparsed
            } else {
                EntityKind::External
            }
        };
        pos = skip_spaces(s, pos);
        if s.get(pos) != Some(&'>') {
            return syntax(pos, "syntax error");
        }
        if !parameter && self.dtd.keep_processing {
            self.dtd.entities.entry(name).or_insert(kind);
        }
        Ok(pos + 1)
    }

    /// An entity value literal: character references expanded, general entity
    /// references kept, parameter entity references not allowed in the internal subset.
    fn entity_value(&mut self, s: Source<'_>, index: usize) -> Result<(Vec<char>, usize), XmlError> {
        let quote = s[index];
        let mut pos = index + 1;
        let mut text = Vec::new();
        loop {
            match s.get(pos) {
                None => return syntax(index, "unclosed token"),
                Some(&c) if c == quote => return Ok((text, pos + 1)),
                Some('%') => {
                    reference_name(s, pos, '%')?;
                    return syntax(pos, "illegal parameter entity reference");
                }
                Some('&') if s.get(pos + 1) == Some(&'#') => {
                    let (c, end) = char_reference(s, pos)?;
                    text.push(c);
                    pos = end;
                }
                Some('&') => {
                    let (_, end) = entity_reference_name(s, pos)?;
                    text.extend_from_slice(&s[pos..end]);
                    pos = end;
                }
                Some(&c) if is_xml_char(c) => {
                    text.push(c);
                    pos += 1;
                }
                Some(_) => return syntax(pos, "not well-formed (invalid token)"),
            }
        }
    }

    fn attlist_declaration(&mut self, s: Source<'_>, index: usize) -> Result<usize, XmlError> {
        let mut pos = require_spaces(s, index + "<!ATTLIST".len())?;
        let (element, after) = prolog_name(s, pos)?;
        pos = after;
        loop {
            let spaced = s.get(pos).copied().is_some_and(is_space);
            pos = skip_spaces(s, pos);
            if s.get(pos) == Some(&'>') {
                return Ok(pos + 1);
            }
            if !spaced {
                return syntax(pos, "syntax error");
            }
            let (name, after) = prolog_name(s, pos)?;
            pos = require_spaces(s, after)?;
            let (is_cdata, is_id, after) = attribute_type(s, pos)?;
            pos = require_spaces(s, after)?;
            let default = if starts_with(s, pos, "#REQUIRED") {
                pos += "#REQUIRED".len();
                None
            } else if starts_with(s, pos, "#IMPLIED") {
                pos += "#IMPLIED".len();
                None
            } else {
                if starts_with(s, pos, "#FIXED") {
                    pos = require_spaces(s, pos + "#FIXED".len())?;
                }
                let (value, after) = self.attribute_value(s, pos, is_cdata)?;
                pos = after;
                Some(value)
            };
            if self.dtd.keep_processing {
                let declared = self.dtd.attributes.entry(element.clone()).or_default();
                // Attribute definition: a declaration with a value (or of type ID) is ignored when the attribute
                // was declared before; the others are appended.
                let known = declared.iter().any(|existing| existing.name == name);
                if !((default.is_some() || is_id) && known) {
                    declared.push(DeclaredAttribute { name, default, is_cdata });
                }
            }
        }
    }
}

/// Checks of a namespace binding (reserved prefixes and names).
fn check_binding(prefix: Option<&str>, uri: &str, at: usize) -> Result<(), XmlError> {
    if uri.is_empty() && prefix.is_some() {
        return syntax(at, "must not undeclare prefix");
    }
    if prefix == Some("xmlns") {
        return syntax(at, "reserved prefix (xmlns) must not be declared or undeclared");
    }
    if uri.contains(NAMESPACE_SEPARATOR) {
        return syntax(at, "syntax error");
    }
    let must_be_xml = prefix == Some("xml");
    let is_xml = uri == XML_NAMESPACE;
    if must_be_xml != is_xml {
        return syntax(
            at,
            if must_be_xml {
                "reserved prefix (xml) must not be undeclared or bound to another namespace name"
            } else {
                "prefix must not be bound to one of the reserved namespace names"
            },
        );
    }
    if uri == XMLNS_NAMESPACE {
        return syntax(at, "prefix must not be bound to one of the reserved namespace names");
    }
    Ok(())
}

fn starts_with(s: Source<'_>, index: usize, text: &str) -> bool {
    text.chars().enumerate().all(|(offset, c)| s.get(index + offset) == Some(&c))
}

fn skip_spaces(s: Source<'_>, mut index: usize) -> usize {
    while s.get(index).copied().is_some_and(is_space) {
        index += 1;
    }
    index
}

fn require_spaces(s: Source<'_>, index: usize) -> Result<usize, XmlError> {
    if !s.get(index).copied().is_some_and(is_space) {
        return syntax(index, "syntax error");
    }
    Ok(skip_spaces(s, index))
}

/// A name without a colon (`NCName`).
fn nc_name(s: Source<'_>, index: usize) -> Result<(String, usize), XmlError> {
    if !s.get(index).copied().is_some_and(is_name_start) {
        return syntax(index, "not well-formed (invalid token)");
    }
    let mut end = index + 1;
    while s.get(end).copied().is_some_and(is_name_char) {
        end += 1;
    }
    if s.get(end) == Some(&':') {
        return syntax(end, "not well-formed (invalid token)");
    }
    Ok((s[index..end].iter().collect(), end))
}

/// A qualified name: an NCName, optionally `:` and a second NCName.
fn qualified_name(s: Source<'_>, index: usize) -> Result<(String, usize), XmlError> {
    let local = |start: usize| {
        let mut end = start;
        if !s.get(end).copied().is_some_and(is_name_start) {
            return Err(XmlError::Syntax { offset: end, reason: "not well-formed (invalid token)" });
        }
        end += 1;
        while s.get(end).copied().is_some_and(is_name_char) {
            end += 1;
        }
        Ok(end)
    };
    let mut end = local(index)?;
    if s.get(end) == Some(&':') {
        end = local(end + 1)?;
        if s.get(end) == Some(&':') {
            return syntax(end, "not well-formed (invalid token)");
        }
    }
    Ok((s[index..end].iter().collect(), end))
}

/// A qualified name in a declaration: as in content, except that the part after the colon may start with any
/// name character.
fn prolog_name(s: Source<'_>, index: usize) -> Result<(String, usize), XmlError> {
    if !s.get(index).copied().is_some_and(is_name_start) {
        return syntax(index, "syntax error");
    }
    let mut end = index + 1;
    while s.get(end).copied().is_some_and(is_name_char) {
        end += 1;
    }
    if s.get(end) == Some(&':') {
        let local = end + 1;
        end = local;
        while s.get(end).copied().is_some_and(is_name_char) {
            end += 1;
        }
        if end == local || s.get(end) == Some(&':') {
            return syntax(end, "syntax error");
        }
    }
    Ok((s[index..end].iter().collect(), end))
}

/// `&name;` / `%name;` starting at `index`: the name and the index after `;`.
fn reference_name(s: Source<'_>, index: usize, sigil: char) -> Result<(String, usize), XmlError> {
    debug_assert_eq!(s.get(index), Some(&sigil));
    let (name, end) = nc_name(s, index + 1)?;
    if s.get(end) != Some(&';') {
        return syntax(end, "not well-formed (invalid token)");
    }
    Ok((name, end + 1))
}

fn entity_reference_name(s: Source<'_>, index: usize) -> Result<(String, usize), XmlError> {
    reference_name(s, index, '&')
}

/// `&#N;` / `&#xH;` (lower-case `x`), a valid XML character.
fn char_reference(s: Source<'_>, index: usize) -> Result<(char, usize), XmlError> {
    let mut pos = index + 2;
    let hex = s.get(pos) == Some(&'x');
    if hex {
        pos += 1;
    }
    let start = pos;
    while s.get(pos).is_some_and(|c| if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() }) {
        pos += 1;
    }
    if pos == start || s.get(pos) != Some(&';') {
        return syntax(pos, "not well-formed (invalid token)");
    }
    let digits: String = s[start..pos].iter().collect();
    let value = u32::from_str_radix(&digits, if hex { 16 } else { 10 }).ok();
    match value.and_then(char::from_u32).filter(|c| is_xml_char(*c)) {
        Some(c) => Ok((c, pos + 1)),
        None => syntax(index, "reference to invalid character number"),
    }
}

fn predefined(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

/// `<!--` … `-->` without `--` inside.
fn comment(s: Source<'_>, index: usize) -> Result<usize, XmlError> {
    let mut pos = index + 4;
    loop {
        match s.get(pos) {
            None => return syntax(index, "unclosed token"),
            Some('-') if s.get(pos + 1) == Some(&'-') => {
                return if s.get(pos + 2) == Some(&'>') {
                    Ok(pos + 3)
                } else {
                    syntax(pos, "not well-formed (invalid token)")
                };
            }
            Some(&c) if is_xml_char(c) => pos += 1,
            Some(_) => return syntax(pos, "not well-formed (invalid token)"),
        }
    }
}

/// `<?target …?>`; the target `xml` is a misplaced declaration, other spellings of it are invalid.
fn processing_instruction(s: Source<'_>, index: usize) -> Result<usize, XmlError> {
    let (target, mut pos) = nc_name(s, index + 2)?;
    if target == "xml" {
        return syntax(index, "XML or text declaration not at start of entity");
    }
    if target.eq_ignore_ascii_case("xml") {
        return syntax(index, "not well-formed (invalid token)");
    }
    if starts_with(s, pos, "?>") {
        return Ok(pos + 2);
    }
    if !s.get(pos).copied().is_some_and(is_space) {
        return syntax(pos, "not well-formed (invalid token)");
    }
    loop {
        match s.get(pos) {
            None => return syntax(index, "unclosed token"),
            Some('?') if s.get(pos + 1) == Some(&'>') => return Ok(pos + 2),
            Some(&c) if is_xml_char(c) => pos += 1,
            Some(_) => return syntax(pos, "not well-formed (invalid token)"),
        }
    }
}

/// A quoted literal; `pubid` restricts the characters to `PubidChar`.
fn literal(s: Source<'_>, index: usize, pubid: bool) -> Result<usize, XmlError> {
    let Some(&quote) = s.get(index).filter(|c| **c == '"' || **c == '\'') else {
        return syntax(index, "syntax error");
    };
    let mut pos = index + 1;
    loop {
        match s.get(pos) {
            None => return syntax(index, "unclosed token"),
            Some(&c) if c == quote => return Ok(pos + 1),
            Some(&c) if pubid && !(c.is_ascii_alphanumeric() || " \n\r-'()+,./:=?;!*#@$_%".contains(c)) => {
                return syntax(pos, "illegal character(s) in public id");
            }
            Some(&c) if is_xml_char(c) => pos += 1,
            Some(_) => return syntax(pos, "not well-formed (invalid token)"),
        }
    }
}

/// `SYSTEM "…"` or `PUBLIC "…" "…"` (the system literal optional for a notation); returns the index after it
/// and whether a system literal was present.
fn external_id(s: Source<'_>, index: usize, notation: bool) -> Result<(usize, bool), XmlError> {
    if starts_with(s, index, "SYSTEM") {
        let pos = require_spaces(s, index + "SYSTEM".len())?;
        return Ok((literal(s, pos, false)?, true));
    }
    if starts_with(s, index, "PUBLIC") {
        let pos = require_spaces(s, index + "PUBLIC".len())?;
        let pos = literal(s, pos, true)?;
        let spaced = s.get(pos).copied().is_some_and(is_space);
        let next = skip_spaces(s, pos);
        if spaced && matches!(s.get(next), Some('"' | '\'')) {
            return Ok((literal(s, next, false)?, true));
        }
        if notation {
            return Ok((pos, false));
        }
        return syntax(next, "syntax error");
    }
    syntax(index, "syntax error")
}

fn notation_declaration(s: Source<'_>, index: usize) -> Result<usize, XmlError> {
    let pos = require_spaces(s, index + "<!NOTATION".len())?;
    let (_, after) = nc_name(s, pos).or_else(|_| syntax(pos, "syntax error"))?;
    let pos = require_spaces(s, after)?;
    let (pos, _) = external_id(s, pos, true)?;
    let pos = skip_spaces(s, pos);
    if s.get(pos) != Some(&'>') {
        return syntax(pos, "syntax error");
    }
    Ok(pos + 1)
}

/// The attribute type of an `ATTLIST` definition: (is CDATA, is ID, index after it).
fn attribute_type(s: Source<'_>, index: usize) -> Result<(bool, bool, usize), XmlError> {
    for (keyword, is_cdata, is_id) in [
        ("CDATA", true, false),
        ("IDREFS", false, false),
        ("IDREF", false, false),
        ("ID", false, true),
        ("ENTITY", false, false),
        ("ENTITIES", false, false),
        ("NMTOKENS", false, false),
        ("NMTOKEN", false, false),
    ] {
        let end = index + keyword.chars().count();
        if starts_with(s, index, keyword) && !s.get(end).copied().is_some_and(is_name_char) {
            return Ok((is_cdata, is_id, end));
        }
    }
    let (pos, names) = if starts_with(s, index, "NOTATION") {
        (require_spaces(s, index + "NOTATION".len())?, true)
    } else {
        (index, false)
    };
    if s.get(pos) != Some(&'(') {
        return syntax(pos, "syntax error");
    }
    let mut pos = pos + 1;
    loop {
        pos = skip_spaces(s, pos);
        if names {
            // NOTATION enumerations hold notation names (no colon).
            pos = nc_name(s, pos).or_else(|_| syntax(pos, "syntax error"))?.1;
        } else {
            if !s.get(pos).copied().is_some_and(|c| is_name_char(c) || c == ':') {
                return syntax(pos, "syntax error");
            }
            while s.get(pos).copied().is_some_and(|c| is_name_char(c) || c == ':') {
                pos += 1;
            }
        }
        pos = skip_spaces(s, pos);
        match s.get(pos) {
            Some('|') => pos += 1,
            Some(')') => return Ok((false, false, pos + 1)),
            _ => return syntax(pos, "syntax error"),
        }
    }
}

/// `<!ELEMENT name contentspec>`: EMPTY, ANY, mixed content or a children model (syntax only).
fn element_declaration(s: Source<'_>, index: usize) -> Result<usize, XmlError> {
    let pos = require_spaces(s, index + "<!ELEMENT".len())?;
    let (_, after) = prolog_name(s, pos)?;
    let mut pos = require_spaces(s, after)?;
    if starts_with(s, pos, "EMPTY") {
        pos += 5;
    } else if starts_with(s, pos, "ANY") {
        pos += 3;
    } else if s.get(pos) == Some(&'(') {
        let inner = skip_spaces(s, pos + 1);
        if starts_with(s, inner, "#PCDATA") {
            pos = mixed_content(s, inner + "#PCDATA".len())?;
        } else {
            pos = content_particle_group(s, pos)?;
            if matches!(s.get(pos), Some('?' | '*' | '+')) {
                pos += 1;
            }
        }
    } else {
        return syntax(pos, "syntax error");
    }
    let pos = skip_spaces(s, pos);
    if s.get(pos) != Some(&'>') {
        return syntax(pos, "syntax error");
    }
    Ok(pos + 1)
}

/// `(#PCDATA (| Name)*)*` or `(#PCDATA)`, starting after `#PCDATA`.
fn mixed_content(s: Source<'_>, mut pos: usize) -> Result<usize, XmlError> {
    let mut names = false;
    loop {
        pos = skip_spaces(s, pos);
        match s.get(pos) {
            Some(')') => {
                if s.get(pos + 1) == Some(&'*') {
                    return Ok(pos + 2);
                }
                return if names { syntax(pos + 1, "syntax error") } else { Ok(pos + 1) };
            }
            Some('|') => {
                pos = skip_spaces(s, pos + 1);
                let (_, after) = prolog_name(s, pos)?;
                pos = after;
                names = true;
            }
            _ => return syntax(pos, "syntax error"),
        }
    }
}

/// A choice or sequence group `( cp (sep cp)* )` with one kind of separator.
fn content_particle_group(s: Source<'_>, index: usize) -> Result<usize, XmlError> {
    let mut pos = index + 1;
    let mut separator: Option<char> = None;
    loop {
        pos = skip_spaces(s, pos);
        if s.get(pos) == Some(&'(') {
            pos = content_particle_group(s, pos)?;
        } else {
            let (_, after) = prolog_name(s, pos)?;
            pos = after;
        }
        if matches!(s.get(pos), Some('?' | '*' | '+')) {
            pos += 1;
        }
        pos = skip_spaces(s, pos);
        match s.get(pos) {
            Some(')') => return Ok(pos + 1),
            Some(&c @ ('|' | ',')) if separator.is_none_or(|existing| existing == c) => {
                separator = Some(c);
                pos += 1;
            }
            _ => return syntax(pos, "syntax error"),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Indentation and serialization.

/// Unit of indentation.
const INDENT: &str = "    ";

/// Pretty-prints in place: blank text and tails are replaced by a newline and four spaces per level.
pub fn indent(element: &mut Element) {
    indent_at(element, 0);
}

fn indent_at(element: &mut Element, level: usize) {
    let newline = format!("\n{}", INDENT.repeat(level));
    if element.children.is_empty() {
        if level > 0 && is_blank(&element.tail) {
            element.tail = Some(newline);
        }
        return;
    }
    if is_blank(&element.text) {
        element.text = Some(format!("{newline}{INDENT}"));
    }
    if is_blank(&element.tail) {
        element.tail = Some(newline.clone());
    }
    for child in &mut element.children {
        indent_at(child, level + 1);
    }
    if let Some(last) = element.children.last_mut() {
        if is_blank(&last.tail) {
            last.tail = Some(newline);
        }
    }
}

fn is_blank(data: &Option<String>) -> bool {
    data.as_deref().is_none_or(|text| text.chars().all(is_py_space))
}

/// The prefixes given to well-known namespaces.
const WELL_KNOWN_PREFIXES: [(&str, &str); 7] = [
    (XML_NAMESPACE, "xml"),
    ("http://www.w3.org/1999/xhtml", "html"),
    ("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf"),
    ("http://schemas.xmlsoap.org/wsdl/", "wsdl"),
    ("http://www.w3.org/2001/XMLSchema", "xs"),
    ("http://www.w3.org/2001/XMLSchema-instance", "xsi"),
    ("http://purl.org/dc/elements/1.1/", "dc"),
];

/// Namespace collection: the serialized name of every `{uri}local` name, and the namespaces declared on
/// the root (`ns0`, `ns1`, … in order of first use, well-known prefixes kept, `xml` never declared).
#[derive(Default)]
struct Names {
    qnames: HashMap<String, String>,
    namespaces: Vec<(String, String)>,
}

impl Names {
    fn of(root: &Element) -> Names {
        let mut names = Names::default();
        let mut stack = vec![root];
        while let Some(element) = stack.pop() {
            names.add(&element.tag);
            for (key, _) in &element.attributes {
                names.add(key);
            }
            stack.extend(element.children.iter().rev());
        }
        names
    }

    fn add(&mut self, name: &str) {
        if self.qnames.contains_key(name) {
            return;
        }
        let qualified = match name.strip_prefix('{').and_then(|rest| rest.rsplit_once('}')) {
            Some((uri, local)) => {
                let known = self.namespaces.iter().find(|(existing, _)| existing == uri).map(|(_, p)| p.clone());
                let prefix = match known {
                    Some(prefix) => prefix,
                    None => {
                        let prefix = WELL_KNOWN_PREFIXES
                            .iter()
                            .find(|(known_uri, _)| *known_uri == uri)
                            .map_or_else(|| format!("ns{}", self.namespaces.len()), |(_, p)| (*p).to_owned());
                        if prefix != "xml" {
                            self.namespaces.push((uri.to_owned(), prefix.clone()));
                        }
                        prefix
                    }
                };
                format!("{prefix}:{local}")
            }
            None => name.to_owned(),
        };
        self.qnames.insert(name.to_owned(), qualified);
    }

    fn qualified<'a>(&'a self, name: &'a str) -> &'a str {
        self.qnames.get(name).map_or(name, String::as_str)
    }
}

/// Serializes an element and its tail as text.
pub fn to_string(element: &Element) -> String {
    let names = Names::of(element);
    let mut out = String::new();
    write_element(&mut out, element, &names, true);
    out
}

/// The whole document with a UTF-8 XML declaration, before the
/// text-mode line-end translation.
pub fn document_text(root: &Element) -> String {
    format!("<?xml version='1.0' encoding='UTF-8'?>\n{}", to_string(root))
}

/// Serializes one element, empty elements in short form; namespace declarations go on the root, sorted by prefix.
fn write_element(out: &mut String, element: &Element, names: &Names, root: bool) {
    let tag = names.qualified(&element.tag);
    out.push('<');
    out.push_str(tag);
    if root {
        let mut declarations: Vec<&(String, String)> = names.namespaces.iter().collect();
        declarations.sort_by(|a, b| a.1.cmp(&b.1));
        for (uri, prefix) in declarations {
            let prefix = if prefix.is_empty() { String::new() } else { format!(":{prefix}") };
            out.push_str(&format!(" xmlns{prefix}=\"{}\"", escape_attribute(uri)));
        }
    }
    for (name, value) in &element.attributes {
        out.push_str(&format!(" {}=\"{}\"", names.qualified(name), escape_attribute(value)));
    }
    let text = element.text.as_deref().unwrap_or_default();
    if text.is_empty() && element.children.is_empty() {
        out.push_str(" />");
    } else {
        out.push('>');
        out.push_str(&escape_text(text));
        element.children.iter().for_each(|child| write_element(out, child, names, false));
        out.push_str(&format!("</{tag}>"));
    }
    out.push_str(&escape_text(element.tail.as_deref().unwrap_or_default()));
}

fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn escape_attribute(value: &str) -> String {
    escape_text(value).replace('"', "&quot;").replace('\r', "&#13;").replace('\n', "&#10;").replace('\t', "&#09;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parsing: comments dropped (surrounding text joined), entities and CDATA decoded, attribute whitespace
    /// normalized, CRLF normalized; well-formedness errors.
    #[test]
    fn parse_like_expat() {
        let root = parse_str(
            "<?xml version=\"1.0\"?>\r\n<!-- c --><a x='1&amp;2' y=\"a\tb\">t<!-- c -->u<b/>&lt;tail<![CDATA[<x>]]></a>\n",
        )
        .unwrap();
        assert_eq!(root.tag, "a");
        assert_eq!(root.attribute("x"), Some("1&2"));
        assert_eq!(root.attribute("y"), Some("a b"));
        assert_eq!(root.text.as_deref(), Some("tu"));
        assert_eq!(root.children[0].tail.as_deref(), Some("<tail<x>"));
        for bad in [
            "<a><b></a>",
            "<a x='1' x='2'/>",
            "<a>&nbsp;</a>",
            "<a/><b/>",
            "<a>]]></a>",
            "<a><!-- a--b --></a>",
            "<a x=\"1\"y=\"2\"/>",
            "<a>&#X41;</a>",
            "<a>\u{1}</a>",
            " <?xml version=\"1.0\"?><a/>",
            "<a><?xml x?></a>",
        ] {
            assert!(parse_str(bad).is_err(), "{bad:?}");
        }
    }

    /// Parsing file bytes: byte-order marks, declared encodings handled directly or through the single-byte map,
    /// UTF-16 detection, multi-byte and mismatched encodings refused.
    #[test]
    fn parse_file_encodings() {
        assert_eq!(parse_bytes(b"\xef\xbb\xbf<a/>").unwrap().tag, "a");
        let latin = parse_bytes(b"<?xml version='1.0' encoding='ISO-8859-1'?><a v='\xe9'/>").unwrap();
        assert_eq!(latin.attribute("v"), Some("\u{e9}"));
        let cp1252 = parse_bytes(b"<?xml version='1.0' encoding='windows-1252'?><a>\x80</a>").unwrap();
        assert_eq!(cp1252.text.as_deref(), Some("\u{20ac}"));
        assert!(parse_bytes(b"<?xml version='1.0' encoding='cp1252'?><a>\x81</a>").is_err());
        assert!(parse_bytes(b"<?xml version='1.0' encoding='utf8'?><a>\xc3\xa9</a>").is_err());
        assert!(matches!(parse_bytes(b"<?xml version='1.0' encoding='EUC-JP'?><a/>"), Err(XmlError::Encoding(_))));
        assert!(parse_bytes(b"<?xml version='1.0' encoding='UTF-16'?><a/>").is_err());
        assert_eq!(parse_bytes(b"\xff\xfe<\x00a\x00/\x00>\x00").unwrap().tag, "a");
        assert_eq!(parse_bytes(b"<\x00a\x00/\x00>\x00").unwrap().tag, "a");
    }

    /// Namespaces become `{uri}local`; serialization declares ns0, ns1… (well-known prefixes kept) on the root.
    #[test]
    fn namespaces() {
        let root = parse_str("<n:a xmlns:n=\"u\" n:x=\"1\" y=\"2\"><b xmlns=\"v\" xml:lang=\"en\"/></n:a>").unwrap();
        assert_eq!(root.tag, "{u}a");
        assert_eq!(root.attributes, [("{u}x".to_owned(), "1".to_owned()), ("y".to_owned(), "2".to_owned())]);
        assert_eq!(root.children[0].tag, "{v}b");
        assert_eq!(
            to_string(&root),
            "<ns0:a xmlns:ns0=\"u\" xmlns:ns1=\"v\" ns0:x=\"1\" y=\"2\"><ns1:b xml:lang=\"en\" /></ns0:a>"
        );
        for bad in [
            "<a b:c='1'/>",
            "<a xmlns:p=''/>",
            "<a xmlns:xml='u'/>",
            "<a xmlns:xmlns='u'/>",
            "<a xmlns='u}v'/>",
            "<a xmlns:p='u' xmlns:q='u' p:x='1' q:x='2'/>",
            "<a:b:c xmlns:a='u'/>",
        ] {
            assert!(parse_str(bad).is_err(), "{bad:?}");
        }
        let xsi = parse_str("<a xmlns:x='http://www.w3.org/2001/XMLSchema-instance' x:c='2'/>").unwrap();
        assert_eq!(to_string(&xsi), "<a xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:c=\"2\" />");
    }

    /// The internal subset: entities (with markup), attribute defaults and tokenized normalization, and the
    /// effect of an unread parameter entity or external subset.
    #[test]
    fn internal_subset() {
        let root = parse_str(
            "<!DOCTYPE a [<!ENTITY e \"<b>1</b>\"><!ATTLIST a x CDATA \"d\" y NMTOKENS \"  p   q \">]><a>&e;</a>",
        )
        .unwrap();
        assert_eq!(root.attributes, [("x".to_owned(), "d".to_owned()), ("y".to_owned(), "p q".to_owned())]);
        assert_eq!(root.children[0].tag, "b");
        assert!(parse_str("<!DOCTYPE a [<!ENTITY e \"<b>\">]><a>&e;</b></a>").is_err());
        assert!(parse_str("<!DOCTYPE a [<!ENTITY e \"&f;\"><!ENTITY f \"&e;\">]><a>&e;</a>").is_err());
        assert!(parse_str("<!DOCTYPE a [<!ENTITY % p \"x\"> %p; <!ENTITY e \"v\">]><a>&e;</a>").is_err());
        let dropped = parse_str("<!DOCTYPE a SYSTEM \"x.dtd\"><a x=\"&u;\"/>").unwrap();
        assert_eq!(dropped.attribute("x"), Some(""));
        let defaulted = parse_str("<!DOCTYPE a [<!ATTLIST a xmlns CDATA \"urn:x\">]><a/>").unwrap();
        assert_eq!(defaulted.tag, "{urn:x}a");
    }

    /// Indent and write: 4-space indentation, " />" for empty elements, escaping.
    #[test]
    fn indent_and_serialize() {
        let mut root = parse_str("<hardware_settings_config><motion><udp port=\"1\" ip=\"a&quot;b\"/></motion><x>keep</x></hardware_settings_config>").unwrap();
        root.children[0].children.push(Element::new("udp", vec![("port".into(), "2".into())]));
        indent(&mut root);
        assert_eq!(
            document_text(&root),
            "<?xml version='1.0' encoding='UTF-8'?>\n<hardware_settings_config>\n    <motion>\n        \
<udp port=\"1\" ip=\"a&quot;b\" />\n        <udp port=\"2\" />\n    </motion>\n    <x>keep</x>\n\
</hardware_settings_config>\n"
        );
    }

    /// Descendant lookup: first descendant in document order.
    #[test]
    fn descendant_lookup() {
        let root = parse_str("<t><a><parameters n='1'/></a><parameters n='2'/></t>").unwrap();
        let path = root.find_descendant_path("parameters").unwrap();
        assert_eq!(path, [0, 0]);
        assert_eq!(parse_str("<t/>").unwrap().find_descendant_path("parameters"), None);
    }
}

#[cfg(test)]
mod oracle {
    use super::*;

    fn dump(root: &Element) -> serde_json::Value {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(element) = stack.pop() {
            let attributes: Vec<serde_json::Value> =
                element.attributes.iter().map(|(k, v)| serde_json::json!([k, v])).collect();
            out.push(serde_json::json!([element.tag, attributes, element.text, element.tail]));
            stack.extend(element.children.iter().rev());
        }
        serde_json::Value::Array(out)
    }

    /// A table of parse/serialize cases with their expected results.
    #[test]
    #[ignore]
    fn matches_python() {
        let Ok(path) = std::env::var("OPENRIGCUE_XML_ORACLE") else { return };
        let cases: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut failures = 0;
        for case in &cases {
            let (input, result) = if case["kind"] == "bytes" {
                let hex = case["hex"].as_str().unwrap();
                let bytes: Vec<u8> =
                    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
                (format!("{:?}", String::from_utf8_lossy(&bytes)), parse_bytes(&bytes))
            } else {
                let text = case["text"].as_str().unwrap();
                (format!("str {text:?}"), parse_str(text))
            };
            let ok = match (&result, case.get("ok")) {
                (Ok(root), Some(expected)) => {
                    dump(root) == *expected && to_string(root) == case["ser"].as_str().unwrap()
                }
                (Err(_), None) => true,
                _ => false,
            };
            if !ok {
                failures += 1;
                if failures <= 60 {
                    eprintln!(
                        "MISMATCH {input}\n  ours: {:?}\n  python: {}",
                        result.as_ref().map(|r| (dump(r), to_string(r))),
                        case
                    );
                }
            }
        }
        eprintln!("{failures} of {} cases differ", cases.len());
        assert_eq!(failures, 0);
    }
}

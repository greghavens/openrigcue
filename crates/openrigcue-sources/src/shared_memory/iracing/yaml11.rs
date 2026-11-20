//! A YAML 1.1 loader for the session info: the standard safe types (YAML 1.1 resolver and
//! constructors) without the timestamp type.
//!
//! The parser is `yaml-rust2`'s event parser; the document is built here with these rules: a plain scalar is
//! typed by the 1.1 implicit resolvers (`yes`/`no`/`on`/`off` booleans, `1_000`, `0o`-less octal `017`, `0x1F`,
//! `0b101`, sexagesimal `1:20`, floats only with a dot, `.inf`, `.nan`, `~`, empty), a quoted scalar is a string,
//! an explicit `!!str`/`!!int`/`!!float`/`!!bool`/`!!null` tag forces the type, `<<` merges mappings, and a stream
//! with more than one document is an error.

use openrigcue_model::{native_value::LegacyValue, python_compare::native_equal};
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};
use yaml_rust2::yaml::{Hash, Yaml};

use super::Raised;

/// Loads the single document of `source`; `Ok(None)` for an empty stream, `Err` for a document that cannot be loaded.
pub(crate) fn load(source: &str) -> Result<Option<Yaml>, Raised> {
    let mut builder = Builder::default();
    let mut parser = Parser::new_from_str(source);
    parser.load(&mut builder, true).map_err(|_| Raised)?;
    if builder.failed {
        return Err(Raised);
    }
    let mut documents = builder.documents.into_iter();
    let first = documents.next();
    if documents.next().is_some() {
        return Err(Raised);
    }
    Ok(first)
}

/// A node under construction.
enum Open {
    Sequence(Vec<Yaml>, usize),
    /// Entries, the key waiting for its value, the anchor, and the mappings to merge (`<<`).
    Mapping(Vec<(Yaml, Yaml)>, Option<Yaml>, usize, Vec<Yaml>),
}

#[derive(Default)]
struct Builder {
    documents: Vec<Yaml>,
    stack: Vec<Open>,
    anchors: Vec<(usize, Yaml)>,
    failed: bool,
}

/// The `<<` key of a mapping (the merge resolver), kept apart until the mapping ends.
const MERGE_KEY: &str = "\u{0}<<merge>>";

impl Builder {
    fn push_value(&mut self, value: Yaml, anchor: usize) {
        if anchor > 0 {
            self.anchors.push((anchor, value.clone()));
        }
        match self.stack.last_mut() {
            None => self.documents.push(value),
            Some(Open::Sequence(items, _)) => items.push(value),
            Some(Open::Mapping(entries, key, _, merges)) => match key.take() {
                None => *key = Some(value),
                Some(Yaml::String(name)) if name == MERGE_KEY => match value {
                    Yaml::Hash(_) => merges.push(value),
                    Yaml::Array(items) if items.iter().all(|item| matches!(item, Yaml::Hash(_))) => {
                        merges.extend(items)
                    }
                    _ => self.failed = true,
                },
                Some(key) => entries.push((key, value)),
            },
        }
    }

    fn scalar(&mut self, value: String, style: TScalarStyle, tag: Option<(String, String)>) -> Option<Yaml> {
        let plain = style == TScalarStyle::Plain;
        let resolved = match tag {
            Some((handle, suffix)) => match (handle.as_str(), suffix.as_str()) {
                ("!!", "str") | ("tag:yaml.org,2002:", "str") | ("!", "") => Some(Yaml::String(value)),
                ("!!", kind) | ("tag:yaml.org,2002:", kind) => construct(kind, &value),
                _ => None,
            },
            None if plain => {
                if self.expecting_key() && value == "<<" {
                    return Some(Yaml::String(MERGE_KEY.to_owned()));
                }
                match resolve(&value) {
                    Kind::Str => Some(Yaml::String(value)),
                    Kind::Undefined => None,
                    kind => construct(kind.tag(), &value),
                }
            }
            None => Some(Yaml::String(value)),
        };
        resolved
    }

    fn expecting_key(&self) -> bool {
        matches!(self.stack.last(), Some(Open::Mapping(_, None, _, _)))
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, _mark: Marker) {
        if self.failed {
            return;
        }
        match event {
            Event::Scalar(value, style, anchor, tag) => {
                let tag = tag.map(|tag| (tag.handle, tag.suffix));
                match self.scalar(value, style, tag) {
                    Some(value) => self.push_value(value, anchor),
                    None => self.failed = true,
                }
            }
            Event::SequenceStart(anchor, _) => self.stack.push(Open::Sequence(Vec::new(), anchor)),
            Event::MappingStart(anchor, _) => self.stack.push(Open::Mapping(Vec::new(), None, anchor, Vec::new())),
            Event::SequenceEnd => {
                if let Some(Open::Sequence(items, anchor)) = self.stack.pop() {
                    self.push_value(Yaml::Array(items), anchor);
                }
            }
            Event::MappingEnd => {
                if let Some(Open::Mapping(entries, _, anchor, merges)) = self.stack.pop() {
                    self.push_value(Yaml::Hash(flatten(entries, merges)), anchor);
                }
            }
            Event::Alias(id) => match self.anchors.iter().rev().find(|(anchor, _)| *anchor == id) {
                Some((_, value)) => {
                    let value = value.clone();
                    self.push_value(value, 0);
                }
                None => self.failed = true,
            },
            _ => {}
        }
    }
}

/// Builds a mapping: the entries of the merged mappings (the last one
/// first), then the mapping's own entries, assigned in that order (a repeated
/// key keeps its first position and takes the last value).
fn flatten(entries: Vec<(Yaml, Yaml)>, merges: Vec<Yaml>) -> Hash {
    let mut hash = Hash::new();
    let merged = merges.into_iter().rev().flat_map(|mapping| match mapping {
        Yaml::Hash(entries) => entries.into_iter().collect::<Vec<_>>(),
        _ => Vec::new(),
    });
    for (key, value) in merged.chain(entries) {
        // Keys that are numerically equal are the same key. Resolve such aliases during
        // construction, before repeated YAML keys lose their assignment order.
        // Reusing the first key also retains its kind and position.
        let key = if let Some(number) = numeric_key(&key) {
            hash.keys()
                .find(|existing| numeric_key(existing).is_some_and(|other| native_equal(&other, &number)))
                .cloned()
                .unwrap_or(key)
        } else {
            key
        };
        hash.replace(key, value);
    }
    hash
}

fn numeric_key(key: &Yaml) -> Option<LegacyValue> {
    match key {
        Yaml::Boolean(value) => Some(LegacyValue::Bool(*value)),
        Yaml::Integer(value) => Some(LegacyValue::Int(*value)),
        Yaml::Real(_) => key.as_f64().map(LegacyValue::Float),
        _ => None,
    }
}

/// What a plain scalar resolves to (the YAML 1.1 implicit resolvers, timestamp removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Bool,
    Int,
    Float,
    Null,
    /// `tag:yaml.org,2002:value` (`=`), which cannot be constructed.
    Undefined,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Bool => "bool",
            Kind::Int => "int",
            Kind::Float => "float",
            Kind::Null => "null",
            Kind::Str | Kind::Undefined => "str",
        }
    }
}

fn resolve(value: &str) -> Kind {
    let Some(first) = value.chars().next() else { return Kind::Null };
    if "yYnNtTfFoO".contains(first) && is_bool(value) {
        return Kind::Bool;
    }
    if "-+0123456789.".contains(first) && is_float(value) {
        return Kind::Float;
    }
    if "-+0123456789".contains(first) && is_int(value) {
        return Kind::Int;
    }
    if "~nN".contains(first) && matches!(value, "~" | "null" | "Null" | "NULL") {
        return Kind::Null;
    }
    if value == "=" {
        return Kind::Undefined;
    }
    Kind::Str
}

fn is_bool(value: &str) -> bool {
    matches!(
        value,
        "yes"
            | "Yes"
            | "YES"
            | "no"
            | "No"
            | "NO"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
    )
}

fn strip_sign(value: &str) -> &str {
    value.strip_prefix(['-', '+']).unwrap_or(value)
}

/// `[0-9][0-9_]*` from the start; returns the rest.
fn digits_then_underscores(text: &str) -> Option<&str> {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_digit() => {}
        _ => return None,
    }
    let end = text.find(|c: char| !(c.is_ascii_digit() || c == '_')).unwrap_or(text.len());
    Some(&text[end..])
}

/// `(?::[0-5]?[0-9])+` covering the whole of `text`.
fn sexagesimal_tail(mut text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    while !text.is_empty() {
        let Some(rest) = text.strip_prefix(':') else { return false };
        let bytes = rest.as_bytes();
        let len = match bytes {
            [a, b, ..] if (b'0'..=b'5').contains(a) && b.is_ascii_digit() => 2,
            [a, ..] if a.is_ascii_digit() => 1,
            _ => return false,
        };
        text = &rest[len..];
    }
    true
}

/// `[eE][-+][0-9]+` covering the whole of `text`, or nothing.
fn exponent_or_empty(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    let Some(rest) = text.strip_prefix(['e', 'E']) else { return false };
    let Some(digits) = rest.strip_prefix(['-', '+']) else { return false };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `[0-9_]*` from the start; returns the rest.
fn digits_or_underscores(text: &str) -> &str {
    let end = text.find(|c: char| !(c.is_ascii_digit() || c == '_')).unwrap_or(text.len());
    &text[end..]
}

fn is_float(value: &str) -> bool {
    let unsigned = strip_sign(value);
    // [-+]?[0-9][0-9_]*\.[0-9_]*([eE][-+][0-9]+)?
    if let Some(rest) = digits_then_underscores(unsigned) {
        if let Some(fraction) = rest.strip_prefix('.') {
            if exponent_or_empty(digits_or_underscores(fraction)) {
                return true;
            }
        }
        // [-+]?[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*
        if let Some(dot) = rest.find('.') {
            if sexagesimal_tail(&rest[..dot]) && digits_or_underscores(&rest[dot + 1..]).is_empty() {
                return true;
            }
        }
    }
    // \.[0-9][0-9_]*([eE][-+][0-9]+)? (no sign)
    if let Some(fraction) = value.strip_prefix('.') {
        if let Some(rest) = digits_then_underscores(fraction) {
            if exponent_or_empty(rest) {
                return true;
            }
        }
    }
    // [-+]?\.(inf|Inf|INF) and \.(nan|NaN|NAN)
    matches!(unsigned, ".inf" | ".Inf" | ".INF") || matches!(value, ".nan" | ".NaN" | ".NAN")
}

fn is_int(value: &str) -> bool {
    let unsigned = strip_sign(value);
    let all = |text: &str, ok: fn(char) -> bool| !text.is_empty() && text.chars().all(ok);
    if let Some(binary) = unsigned.strip_prefix("0b") {
        return all(binary, |c| c == '0' || c == '1' || c == '_');
    }
    if let Some(hex) = unsigned.strip_prefix("0x") {
        return all(hex, |c| c.is_ascii_hexdigit() || c == '_');
    }
    if let Some(octal) = unsigned.strip_prefix('0') {
        return octal.is_empty() || all(octal, |c| ('0'..='7').contains(&c) || c == '_');
    }
    // [1-9][0-9_]* optionally followed by (:[0-5]?[0-9])+
    match unsigned.chars().next() {
        Some('1'..='9') => {}
        _ => return false,
    }
    let rest = digits_or_underscores(unsigned);
    rest.is_empty() || sexagesimal_tail(rest)
}

/// Constructs a scalar of the resolved kind; `None` where that is an error.
fn construct(kind: &str, value: &str) -> Option<Yaml> {
    match kind {
        "null" => Some(Yaml::Null),
        "bool" => match value.to_lowercase().as_str() {
            "yes" | "true" | "on" => Some(Yaml::Boolean(true)),
            "no" | "false" | "off" => Some(Yaml::Boolean(false)),
            _ => None,
        },
        "int" => construct_int(value),
        "float" => construct_float(value).map(|f| Yaml::Real(real_text(f))),
        _ => None,
    }
}

fn split_sign(value: &str) -> (f64, &str) {
    match value.as_bytes().first() {
        Some(b'-') => (-1.0, &value[1..]),
        Some(b'+') => (1.0, &value[1..]),
        _ => (1.0, value),
    }
}

/// An integer scalar: underscores removed, sign, `0`, `0b`, `0x`, leading `0` (octal), sexagesimal, decimal.
/// A value beyond `i64` is kept as its decimal text, which is how the listener prints it.
fn construct_int(value: &str) -> Option<Yaml> {
    let cleaned = value.replace('_', "");
    let (sign, digits) = split_sign(&cleaned);
    let negative = sign < 0.0;
    let magnitude: Option<i128> = if digits == "0" {
        Some(0)
    } else if let Some(binary) = digits.strip_prefix("0b") {
        i128::from_str_radix(binary, 2).ok()
    } else if let Some(hex) = digits.strip_prefix("0x") {
        i128::from_str_radix(hex, 16).ok()
    } else if digits.starts_with('0') {
        i128::from_str_radix(digits, 8).ok()
    } else if digits.contains(':') {
        digits.split(':').try_fold(0i128, |total, part| part.parse::<i128>().ok().map(|part| total * 60 + part))
    } else {
        digits.parse::<i128>().ok()
    };
    let signed = magnitude? * if negative { -1 } else { 1 };
    Some(match i64::try_from(signed) {
        Ok(small) => Yaml::Integer(small),
        Err(_) => Yaml::String(signed.to_string()),
    })
}

/// A float scalar: underscores removed, lower-cased, sign, `.inf`, `.nan`, sexagesimal, decimal float text.
fn construct_float(value: &str) -> Option<f64> {
    let cleaned = value.replace('_', "").to_lowercase();
    let (sign, digits) = split_sign(&cleaned);
    if digits == ".inf" {
        return Some(sign * f64::INFINITY);
    }
    if digits == ".nan" {
        return Some(f64::NAN);
    }
    if digits.contains(':') {
        let mut total = 0.0;
        for part in digits.split(':') {
            total = total * 60.0 + part.parse::<f64>().ok()?;
        }
        return Some(sign * total);
    }
    let text = if digits.ends_with('.') { format!("{digits}0") } else { digits.to_owned() };
    let text = if text.starts_with('.') { format!("0{text}") } else { text };
    text.parse::<f64>().ok().map(|f| sign * f)
}

/// The text `Yaml::Real` stores for `f`, which `Yaml::as_f64` reads back exactly.
fn real_text(f: f64) -> String {
    if f.is_nan() {
        ".nan".to_owned()
    } else if f.is_infinite() {
        if f > 0.0 { ".inf" } else { "-.inf" }.to_owned()
    } else {
        format!("{f:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(text: &str) -> Yaml {
        let doc = load(&format!("v: {text}\n")).unwrap().unwrap();
        doc["v"].clone()
    }

    /// The YAML 1.1 resolver: booleans, integers in every base, sexagesimal values, floats only with a dot.
    #[test]
    fn plain_scalars_resolve_as_in_pyyaml() {
        assert_eq!(scalar("On"), Yaml::Boolean(true));
        assert_eq!(scalar("No"), Yaml::Boolean(false));
        assert_eq!(scalar("YES"), Yaml::Boolean(true));
        assert_eq!(scalar("yEs"), Yaml::String("yEs".into()));
        assert_eq!(scalar("1_000"), Yaml::Integer(1000));
        assert_eq!(scalar("017"), Yaml::Integer(15));
        assert_eq!(scalar("08"), Yaml::String("08".into()));
        assert_eq!(scalar("0x1F"), Yaml::Integer(31));
        assert_eq!(scalar("0b101"), Yaml::Integer(5));
        assert_eq!(scalar("1:20"), Yaml::Integer(80));
        assert_eq!(scalar("-1:20"), Yaml::Integer(-80));
        assert_eq!(scalar("+12"), Yaml::Integer(12));
        assert_eq!(scalar("1:20.5").as_f64(), Some(80.5));
        assert_eq!(scalar("7400.000").as_f64(), Some(7400.0));
        assert_eq!(scalar("1.").as_f64(), Some(1.0));
        assert_eq!(scalar(".5").as_f64(), Some(0.5));
        assert_eq!(scalar("-.5"), Yaml::String("-.5".into()));
        assert_eq!(scalar("1e5"), Yaml::String("1e5".into()));
        assert_eq!(scalar("1.0e+5").as_f64(), Some(100000.0));
        assert_eq!(scalar("1.0e5"), Yaml::String("1.0e5".into()));
        assert_eq!(scalar("-.inf").as_f64(), Some(f64::NEG_INFINITY));
        assert!(scalar(".NaN").as_f64().unwrap().is_nan());
        assert_eq!(scalar("NaN"), Yaml::String("NaN".into()));
        assert_eq!(scalar("~"), Yaml::Null);
        assert_eq!(scalar("Null"), Yaml::Null);
        assert_eq!(scalar(""), Yaml::Null);
        assert_eq!(scalar("2024-01-02"), Yaml::String("2024-01-02".into()), "timestamp resolver removed");
        assert_eq!(scalar("99999999999999999999"), Yaml::String("99999999999999999999".into()));
    }

    /// Quoted scalars are strings; explicit tags force the type; `=` cannot be constructed.
    #[test]
    fn quoted_tagged_and_undefined() {
        assert_eq!(scalar("'On'"), Yaml::String("On".into()));
        assert_eq!(scalar("\"1_000\""), Yaml::String("1_000".into()));
        assert_eq!(scalar("!!str 12"), Yaml::String("12".into()));
        assert_eq!(scalar("!!int '12'"), Yaml::Integer(12));
        assert!(load("v: =\n").is_err());
        assert!(load("v: !!python/object x\n").is_err());
    }

    /// Numeric aliases retain the first key and the last assigned value, including through merges.
    #[test]
    fn numeric_mapping_aliases_keep_the_first_kind_and_last_assignment() {
        for (source, first) in [
            ("true: first\n1: middle\ntrue: last\n'True': text\n", Yaml::Boolean(true)),
            ("1: first\ntrue: middle\n1: last\n'True': text\n", Yaml::Integer(1)),
            ("1.0: first\ntrue: middle\n1: last\n'True': text\n", Yaml::Real("1.0".into())),
            ("-0.0: first\nfalse: middle\n0: last\n'True': text\n", Yaml::Real("-0.0".into())),
        ] {
            let doc = load(source).unwrap().unwrap();
            assert_eq!(
                doc.as_hash().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect::<Vec<_>>(),
                [(first, Yaml::String("last".into())), (Yaml::String("True".into()), Yaml::String("text".into())),]
            );
        }
        let doc = load("A: &a {true: A, 2: A2}\nB: &b {1: B, 3: B3}\nC:\n  <<: [*a, *b]\n  true: C\n  4: C4\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            doc["C"].as_hash().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect::<Vec<_>>(),
            [
                (Yaml::Integer(1), Yaml::String("C".into())),
                (Yaml::Integer(3), Yaml::String("B3".into())),
                (Yaml::Integer(2), Yaml::String("A2".into())),
                (Yaml::Integer(4), Yaml::String("C4".into())),
            ]
        );
        let doc = load("9007199254740993: wide\n9007199254740992.0: float\n9007199254740992: last\n").unwrap().unwrap();
        assert_eq!(
            doc.as_hash().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect::<Vec<_>>(),
            [
                (Yaml::Integer(9_007_199_254_740_993), Yaml::String("wide".into())),
                (Yaml::Real("9007199254740992.0".into()), Yaml::String("last".into())),
            ]
        );
    }

    /// Mappings, sequences, anchors and merge keys; a second document is an error; an empty stream is None.
    #[test]
    fn structure() {
        let doc =
            load("---\nA:\n - CarIdx: 1\n   CarScreenName: Mazda\nB: &b {x: 1, y: 2}\nC:\n  <<: *b\n  y: 3\n...\n")
                .unwrap()
                .unwrap();
        assert_eq!(doc["A"][0]["CarIdx"], Yaml::Integer(1));
        assert_eq!(doc["A"][0]["CarScreenName"].as_str(), Some("Mazda"));
        assert_eq!((doc["C"]["x"].as_i64(), doc["C"]["y"].as_i64()), (Some(1), Some(3)));
        assert!(load("a: 1\n---\nb: 2\n").is_err());
        assert_eq!(load("").unwrap(), None);
        let repeated = load("k: 1\nj: 2\nk: 3\n").unwrap().unwrap();
        let keys: Vec<_> = repeated.as_hash().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(keys, [(Yaml::String("k".into()), Yaml::Integer(3)), (Yaml::String("j".into()), Yaml::Integer(2))]);
    }
}

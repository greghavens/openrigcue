//! Formatting metadata of a `config.ini` file, kept independently of its dictionary values.

use std::collections::BTreeMap;

use super::{parse_section_marker, parse_triple_quoted, split_key_value, Dict};

#[derive(Debug, Clone, Default)]
pub(super) struct Entry {
    pub comments: Vec<String>,
    pub inline: String,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub(super) indent: String,
    pub(super) newline: String,
    pub(super) bom: bool,
    pub(super) initial: Vec<String>,
    pub(super) final_comments: Vec<String>,
    pub(super) entries: BTreeMap<Vec<String>, Entry>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            indent: "    ".to_owned(),
            newline: "\n".to_owned(),
            bom: false,
            initial: Vec::new(),
            final_comments: Vec::new(),
            entries: BTreeMap::new(),
        }
    }
}

impl Layout {
    /// Capture a source already accepted by the value parser. Physical lines split on LF; a CR-only
    /// input remains one physical line.
    pub fn capture(source: &str) -> Self {
        let bom = source.starts_with('\u{feff}');
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let newline = source.find('\n').map_or_else(
            || if source.ends_with('\r') { "\r" } else { "\n" },
            |i| if i > 0 && source.as_bytes()[i - 1] == b'\r' { "\r\n" } else { "\n" },
        );
        let mut layout = Self { indent: String::new(), newline: newline.to_owned(), bom, ..Self::default() };
        let lines: Vec<&str> = source.lines().collect();
        let mut path: Vec<String> = Vec::new();
        let mut pending = Vec::new();
        let mut first = true;
        let mut index = 0;
        while index < lines.len() {
            let original = lines[index];
            let line = original.trim();
            index += 1;
            if line.is_empty() || line.starts_with('#') {
                pending.push(original.trim_start().to_owned());
                continue;
            }
            if layout.indent.is_empty() {
                let prefix = &original[..original.len() - original.trim_start().len()];
                if !prefix.is_empty() {
                    layout.indent = prefix.to_owned();
                }
            }
            let (entry_path, inline) = if line.starts_with('[') {
                if let Ok((depth, name)) = parse_section_marker(line) {
                    path.truncate(depth - 1);
                    path.push(name);
                    let end = super::marker_end(line).expect("validated marker");
                    (path.clone(), line[end..].to_owned())
                } else {
                    let Ok((key, raw)) = split_key_value(line) else { continue };
                    let mut entry = path.clone();
                    entry.push(key);
                    (entry, inline_comment(raw))
                }
            } else {
                let Ok((key, raw)) = split_key_value(line) else { continue };
                let mut value = raw.to_owned();
                if raw.starts_with("'''") || raw.starts_with("\"\"\"") {
                    if let Ok((_, consumed)) = parse_triple_quoted(raw, &lines[index..]) {
                        for line in &lines[index..index + consumed] {
                            value.push('\n');
                            value.push_str(line);
                        }
                        index += consumed;
                    }
                }
                let mut entry = path.clone();
                entry.push(key);
                (entry, inline_comment(&value))
            };
            if first {
                layout.initial = std::mem::take(&mut pending);
                first = false;
            }
            layout.entries.insert(entry_path, Entry { comments: std::mem::take(&mut pending), inline });
        }
        if first {
            layout.initial = pending;
        } else {
            layout.final_comments = pending;
        }
        layout
    }

    pub fn encode(&self, config: &Dict) -> Result<String, super::IniWriteError> {
        super::write::encode_with_layout(config, self)
    }
}

/// Keep the source whitespace before #; writing an inline comment adds one indent unit.
fn inline_comment(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut index = 0;
    let mut quote: Option<(&str, usize)> = None;
    let mut token_start = true;
    while index < bytes.len() {
        if let Some((delimiter, length)) = quote {
            if raw[index..].starts_with(delimiter) {
                quote = None;
                index += length;
            } else {
                index += raw[index..].chars().next().expect("in bounds").len_utf8();
            }
            continue;
        }
        match bytes[index] {
            b'#' => {
                let start = raw[..index].trim_end().len();
                return raw[start..].to_owned();
            }
            b',' => token_start = true,
            b'\'' | b'"' if token_start => {
                let length = if raw[index..].starts_with("'''") || raw[index..].starts_with("\"\"\"") { 3 } else { 1 };
                quote = Some((&raw[index..index + length], length));
                index += length;
                token_start = false;
                continue;
            }
            b' ' | b'\t' if token_start => {}
            _ => token_start = false,
        }
        index += raw[index..].chars().next().expect("in bounds").len_utf8();
    }
    String::new()
}

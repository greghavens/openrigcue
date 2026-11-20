//! Reader for an imported SRS `config.ini` (UTF-8 INI with comma-separated list values).
//!
//! The result is a [`Dict`] of sections; scalars are strings, comma-separated values are lists of
//! strings and `[[sub]]` sections are nested dicts, the shape the settings migrations consume.

use super::value::{Dict, Key, LegacyValue};

mod layout;
mod write;
pub use layout::Layout;
pub use write::{encode, IniWriteError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("config.ini line {line}: {message}")]
pub struct IniError {
    pub line: usize,
    pub message: String,
}

/// Parses `config.ini` text into nested dicts.
pub fn parse(text: &str) -> Result<Dict, IniError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut root = Dict::new();
    // Path of section names from the root to the section receiving keys.
    let mut path: Vec<String> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let number = index + 1;
        let line = lines[index].trim();
        index += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fail = |message: String| IniError { line: number, message };
        if line.starts_with('[') {
            if let Ok((depth, name)) = parse_section_marker(line) {
                if depth > path.len() + 1 {
                    return Err(fail("section too nested".into()));
                }
                path.truncate(depth - 1);
                let parent = section_mut(&mut root, &path);
                if parent.contains(&name) {
                    return Err(fail(format!("duplicate section {name:?}")));
                }
                parent.insert(name.clone(), LegacyValue::Dict(Dict::new()));
                path.push(name);
                continue;
            }
        }
        let (key, raw) = split_key_value(line).map_err(fail)?;
        let value = if raw.starts_with("'''") || raw.starts_with("\"\"\"") {
            let (value, consumed) = parse_triple_quoted(raw, &lines[index..]).map_err(fail)?;
            index += consumed;
            LegacyValue::Str(value)
        } else {
            parse_value(raw).map_err(fail)?
        };
        let section = section_mut(&mut root, &path);
        if section.contains(&key) {
            return Err(fail(format!("duplicate keyword {key:?}")));
        }
        section.insert(Key::Str(key), value);
    }
    Ok(root)
}

fn section_mut<'a>(root: &'a mut Dict, path: &[String]) -> &'a mut Dict {
    let mut current = root;
    for name in path {
        current = current
            .get_mut(name)
            .and_then(LegacyValue::as_dict_mut)
            .expect("section path only names sections created by the parser");
    }
    current
}

fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0] == bytes[bytes.len() - 1] && (bytes[0] == b'"' || bytes[0] == b'\'') {
        text[1..text.len() - 1].to_owned()
    } else {
        text.to_owned()
    }
}

/// `[name]`, `[[name]]`, … with an optional trailing comment. Returns (depth, unquoted name).
fn parse_section_marker(line: &str) -> Result<(usize, String), String> {
    let end = marker_end(line).ok_or("unclosed section marker")?;
    let without_comment = &line[..end];
    let mut rest = without_comment;
    let mut open = 0;
    while let Some(r) = rest.strip_prefix('[') {
        open += 1;
        rest = r.trim_start();
    }
    let mut close = 0;
    while let Some(r) = rest.strip_suffix(']') {
        close += 1;
        rest = r.trim_end();
    }
    if open != close {
        return Err("cannot compute the section depth".into());
    }
    Ok((open, unquote(rest.trim())))
}

/// The first complete closing bracket run followed only by whitespace or a comment.
fn marker_end(line: &str) -> Option<usize> {
    let mut quoted = None;
    let bytes = line.as_bytes();
    for (i, &byte) in bytes.iter().enumerate() {
        if let Some(quote) = quoted {
            if byte == quote {
                quoted = None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quoted = Some(byte);
        } else if byte == b']' && bytes.get(i + 1) != Some(&b']') {
            let tail = line[i + 1..].trim_start();
            if tail.is_empty() || tail.starts_with('#') {
                return Some(i + 1);
            }
        }
    }
    None
}

fn split_key_value(line: &str) -> Result<(String, &str), String> {
    let first = line.chars().next().expect("caller skips empty lines");
    let (key, after_key) = if first == '"' || first == '\'' {
        let close = line[1..].find(first).ok_or("unterminated quoted keyword")? + 1;
        (line[1..close].to_owned(), &line[close + 1..])
    } else {
        let eq = line.find('=').ok_or("invalid line")?;
        (line[..eq].trim_end().to_owned(), &line[eq..])
    };
    let after_eq = after_key.trim_start().strip_prefix('=').ok_or("invalid line")?;
    if key.is_empty() && first != '"' && first != '\'' {
        return Err("empty keyword".into());
    }
    Ok((key, after_eq.trim_start()))
}

/// One value of the `config.ini` value grammar: a scalar, a comma-separated list (`a, b`, `a,`) or the
/// empty list `,`, each optionally followed by a `#` comment.
fn parse_value(raw: &str) -> Result<LegacyValue, String> {
    let raw = raw.trim_end();
    if let Some(after_comma) = raw.strip_prefix(',') {
        let rest = after_comma.trim_start();
        if rest.is_empty() || rest.starts_with('#') {
            return Ok(LegacyValue::List(Vec::new()));
        }
        return Err("invalid value".into());
    }
    let mut items: Vec<String> = Vec::new();
    let mut is_list = false;
    let mut rest = raw;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with('#') {
            break;
        }
        let (item, after) = take_item(rest)?;
        rest = after.trim_start();
        items.push(item);
        match rest.strip_prefix(',') {
            Some(after_comma) => {
                is_list = true;
                rest = after_comma;
            }
            None if rest.is_empty() || rest.starts_with('#') => break,
            None => return Err("invalid value".into()),
        }
    }
    if is_list {
        Ok(LegacyValue::List(items.into_iter().map(LegacyValue::Str).collect()))
    } else {
        Ok(LegacyValue::Str(items.pop().unwrap_or_default()))
    }
}

/// A quoted item, or an unquoted run up to the next `,` or `#` (trailing whitespace dropped).
fn take_item(text: &str) -> Result<(String, &str), String> {
    let first = text.chars().next().expect("caller checks for empty input");
    if first == '"' || first == '\'' {
        let close = text[1..].find(first).ok_or("unterminated quoted value")? + 1;
        return Ok((text[1..close].to_owned(), &text[close + 1..]));
    }
    let end = text.find([',', '#']).unwrap_or(text.len());
    let item = text[..end].trim_end();
    if item.is_empty() {
        return Err("empty list item".into());
    }
    Ok((item.to_owned(), &text[end..]))
}

/// A `'''`/`"""` value, possibly continuing over the following lines. Returns the value and the
/// number of extra lines consumed.
fn parse_triple_quoted(raw: &str, following: &[&str]) -> Result<(String, usize), String> {
    let quote = &raw[..3];
    let closing = |line: &str| {
        let mut from = 0;
        while let Some(relative) = line[from..].find(quote) {
            let end = from + relative;
            let tail = line[end + 3..].trim_start();
            if tail.is_empty() || tail.starts_with('#') {
                return Some(end);
            }
            from = end + 1;
        }
        None
    };
    let body = &raw[3..];
    if let Some(end) = closing(body) {
        return Ok((body[..end].to_owned(), 0));
    }
    let mut value = body.to_owned();
    for (consumed, line) in following.iter().enumerate() {
        value.push('\n');
        if let Some(end) = closing(line) {
            value.push_str(&line[..end]);
            return Ok((value, consumed + 1));
        }
        value.push_str(line);
    }
    Err("unterminated multi-line value".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> LegacyValue {
        LegacyValue::Str(v.into())
    }

    fn list(items: &[&str]) -> LegacyValue {
        LegacyValue::List(items.iter().map(|i| s(i)).collect())
    }

    /// Value shapes found in `config.ini` files for the values the migrations set.
    #[test]
    fn parses_configobj_output() {
        let text = "\u{feff}# comment\n[APP]\nconfig_file_version = 412\nlog_level = 30\nplaystation_ip = \"\"\n\
            [[trial_started]]\n[SHORTCUTS]\n    power_wind_switch_keys = ctrl+shift+f1\n    \
            \"ejection_seat_btns \" = \"\"\n[LED]\nsegment1_color = 255, 0, 0\n[PT-ACTUATOR]\n\
            udp_config = 192.168.1.1:7408,\nempty = ,\n[MOTION_COMPENSATION]\nrefresh_rate = 90\n\
            [[yaw]]\noffset = 0\nenabled = no\n[[roll]]\noffset = 0\n[Yaw VR]\nname = 'quoted, with comma' # note\n";
        let root = parse(text).unwrap();
        let app = root.get("APP").unwrap().as_dict().unwrap();
        assert_eq!(app.get("config_file_version"), Some(&s("412")));
        assert_eq!(app.get("playstation_ip"), Some(&s("")));
        assert_eq!(app.get("trial_started"), Some(&LegacyValue::Dict(Dict::new())));
        let shortcuts = root.get("SHORTCUTS").unwrap().as_dict().unwrap();
        assert_eq!(shortcuts.get("power_wind_switch_keys"), Some(&s("ctrl+shift+f1")));
        assert_eq!(shortcuts.get("ejection_seat_btns "), Some(&s("")));
        let led = root.get("LED").unwrap().as_dict().unwrap();
        assert_eq!(led.get("segment1_color"), Some(&list(&["255", "0", "0"])));
        let pt = root.get("PT-ACTUATOR").unwrap().as_dict().unwrap();
        assert_eq!(pt.get("udp_config"), Some(&list(&["192.168.1.1:7408"])));
        assert_eq!(pt.get("empty"), Some(&list(&[])));
        let mc = root.get("MOTION_COMPENSATION").unwrap().as_dict().unwrap();
        assert_eq!(mc.str_keys(), ["refresh_rate", "yaw", "roll"]);
        let yaw = mc.get("yaw").unwrap().as_dict().unwrap();
        assert_eq!(yaw.get("enabled"), Some(&s("no")));
        let yaw_vr = root.get("Yaw VR").unwrap().as_dict().unwrap();
        assert_eq!(yaw_vr.get("name"), Some(&s("quoted, with comma")));
    }

    #[test]
    fn unquoted_value_stops_at_comment_and_keeps_inner_spaces() {
        let root = parse("[SHAKEBOX]\nprofile = SRS ShakeKit or SRS Shakebox Elite   # layout\nx =\n").unwrap();
        let sb = root.get("SHAKEBOX").unwrap().as_dict().unwrap();
        assert_eq!(sb.get("profile"), Some(&s("SRS ShakeKit or SRS Shakebox Elite")));
        assert_eq!(sb.get("x"), Some(&s("")));
    }

    #[test]
    fn rejects_malformed_files() {
        assert_eq!(parse("[A]\nk = 1\nk = 2\n").unwrap_err().line, 3);
        assert!(parse("[A]\n[[[B]]]\n").is_err());
        assert!(parse("[A]\nnot a key value\n").is_err());
        assert!(parse("[A]\nk = a,,b\n").is_err());
    }

    #[test]
    fn triple_quoted_values() {
        let root = parse("[A]\nk = '''one\ntwo'''\nj = \"\"\"x\"\"\"\n").unwrap();
        let a = root.get("A").unwrap().as_dict().unwrap();
        assert_eq!(a.get("k"), Some(&s("one\ntwo")));
        assert_eq!(a.get("j"), Some(&s("x")));
    }
}

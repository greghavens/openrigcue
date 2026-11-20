//! `config.ini` writer that keeps the captured layout of scalars, lists and sections. Text stays as
//! Unicode code points until it is encoded as UTF-8 for the file; list items that are builtin
//! objects are converted to their string form before quoting.
use super::{Dict, LegacyValue};
use crate::import::pickle::{utf8_strict, NativeException};
use openrigcue_model::native_value::PythonString;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("config.ini {path}: {message}")]
pub struct IniWriteError {
    pub path: String,
    pub message: String,
    pub native: Option<NativeException>,
}
fn native(path: &str, exception: NativeException) -> IniWriteError {
    IniWriteError { path: path.into(), message: exception.to_string(), native: Some(exception) }
}
fn error(path: &str, message: String) -> IniWriteError {
    native(path, NativeException::new("ConfigObjError", PythonString::from(message.as_str())))
}
fn from_detail(path: &str, detail: String) -> IniWriteError {
    let (class, message) = detail.split_once(": ").unwrap_or(("ValueError", detail.as_str()));
    native(
        path,
        NativeException::new(
            if class == "RecursionError" { "RecursionError" } else { "ValueError" },
            PythonString::from(message),
        ),
    )
}
fn py(text: &str) -> PythonString {
    PythonString::from(text)
}
fn concat(parts: impl IntoIterator<Item = PythonString>) -> PythonString {
    PythonString::from_codepoints(parts.into_iter().flat_map(|v| v.codepoints().to_vec()).collect())
        .expect("code points")
}

pub fn encode(config: &Dict) -> Result<String, IniWriteError> {
    encode_with_layout(config, &super::Layout::default())
}
pub(super) fn encode_with_layout(config: &Dict, layout: &super::Layout) -> Result<String, IniWriteError> {
    let _operation = openrigcue_model::native_object::native_operation();
    let mut lines = layout.initial.iter().map(|v| py(v)).collect::<Vec<_>>();
    section(config, 0, &[], layout, &mut lines, &mut BTreeSet::new())?;
    lines.extend(layout.final_comments.iter().map(|v| py(v)));
    if lines.is_empty() {
        return Ok(String::new());
    }
    let mut output = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        if index != 0 {
            output.extend(layout.newline.chars().map(u32::from));
        }
        output.extend_from_slice(line.codepoints());
    }
    output.extend(layout.newline.chars().map(u32::from));
    let text = PythonString::from_codepoints(output).expect("code points");
    let mut encoded = utf8_strict(&text).map_err(|exception| native("", exception))?;
    if layout.bom {
        encoded.insert(0, '\u{feff}');
    }
    Ok(encoded)
}
fn comments(layout: &super::Layout, path: &[String], depth: usize, out: &mut Vec<PythonString>) -> String {
    let Some(entry) = layout.entries.get(path) else {
        return String::new();
    };
    let indent = layout.indent.repeat(depth);
    out.extend(entry.comments.iter().map(|comment| py(&format!("{indent}{comment}"))));
    if entry.inline.is_empty() {
        String::new()
    } else {
        format!("{}{}", layout.indent, entry.inline)
    }
}
fn section(
    config: &Dict,
    depth: usize,
    path: &[String],
    layout: &super::Layout,
    out: &mut Vec<PythonString>,
    seen: &mut BTreeSet<usize>,
) -> Result<(), IniWriteError> {
    let indent = layout.indent.repeat(depth);
    // Scalars are written first, then sections, each in their own order.
    for (_key, value) in config.iter().filter(|(_, v)| !matches!(v.resolved(), LegacyValue::Dict(_))) {
        let key = _key.to_value().python_string().ok_or_else(|| {
            native(
                &path.join("."),
                NativeException::new("ValueError", py(&format!("The key \"{}\" is not a string.", _key.to_text()))),
            )
        })?;
        let diagnostic = key.scalar_string().unwrap_or_else(|| key.repr());
        let mut entry = path.to_vec();
        entry.push(diagnostic);
        let full = entry.join(".");
        let inline = comments(layout, &entry, depth, out);
        // The value is quoted before its key, so a value error is reported first.
        let value = quoted(value, true, &full)?;
        let name = quote(&key, false, &full)?;
        out.push(concat([py(&indent), name, py(" = "), value, py(&inline)]));
    }
    for (key, value) in config.iter().filter(|(_, v)| matches!(v.resolved(), LegacyValue::Dict(_))) {
        let name = key.to_value().python_string().ok_or_else(|| {
            native(
                &path.join("."),
                NativeException::new("ValueError", py(&format!("The key \"{}\" is not a string.", key.to_text()))),
            )
        })?;
        let diagnostic = name.scalar_string().unwrap_or_else(|| name.repr());
        let mut entry = path.to_vec();
        entry.push(diagnostic);
        let full = entry.join(".");
        let inline = comments(layout, &entry, depth, out);
        let mut title = quote(&name, false, &full)?;
        if !name.is_empty() && title.len() == name.len() + 2 {
            title = name;
        }
        out.push(concat([py(&indent), py(&"[".repeat(depth + 1)), title, py(&"]".repeat(depth + 1)), py(&inline)]));
        let id = if let LegacyValue::Object(object) = value {
            if !seen.insert(object.id()) {
                return Err(from_detail(&full, "RecursionError: maximum recursion depth exceeded".into()));
            }
            Some(object.id())
        } else {
            None
        };
        let resolved = value.resolved();
        let LegacyValue::Dict(values) = resolved else { unreachable!("section") };
        section(&values, depth + 1, &entry, layout, out, seen)?;
        if let Some(id) = id {
            seen.remove(&id);
        }
    }
    Ok(())
}
fn quoted(value: &LegacyValue, multiline: bool, path: &str) -> Result<PythonString, IniWriteError> {
    let value = value.resolved();
    if multiline {
        if let LegacyValue::List(values) | LegacyValue::Tuple(values) = &value {
            if values.is_empty() {
                return Ok(py(","));
            }
            let mut parts = Vec::new();
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    parts.push(py(", "));
                }
                parts.push(quoted(value, false, path)?);
            }
            if values.len() == 1 {
                parts.push(py(","));
            }
            return Ok(concat(parts));
        }
    }
    let text = openrigcue_model::python_repr::str_native(&value).map_err(|detail| from_detail(path, detail))?;
    quote(&text, multiline, path)
}
fn quote(text: &PythonString, multiline: bool, path: &str) -> Result<PythonString, IniWriteError> {
    let points = text.codepoints();
    if points.is_empty() {
        return Ok(py("\"\""));
    }
    let contains = |c: char| points.contains(&u32::from(c));
    let sequence = |p: &[u32]| points.windows(p.len()).any(|v| v == p);
    let failure = || error(path, format!("Value cannot be safely quoted: {}", text.repr()));
    if multiline && (contains('\n') || (contains('\'') && contains('"'))) {
        let delimiter = if !sequence(&[39, 39, 39]) {
            "'''"
        } else if !sequence(&[34, 34, 34]) {
            "\"\"\""
        } else {
            return Err(failure());
        };
        return Ok(concat([py(delimiter), text.clone(), py(delimiter)]));
    }
    if contains('\n') {
        return Err(failure());
    }
    let edge = |cp: &u32| [32, 13, 10, 11, 9, 39, 34].contains(cp);
    if !edge(&points[0]) && !edge(points.last().expect("nonempty")) && !contains(',') && !contains('#') {
        return Ok(text.clone());
    }
    if contains('\'') && contains('"') {
        return Err(failure());
    }
    let delimiter = if contains('"') { "'" } else { "\"" };
    Ok(concat([py(delimiter), text.clone(), py(delimiter)]))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::configobj::parse;

    /// Expected encodings and errors for each fixture case in `configobj-reference.json`.
    #[test]
    fn matches_shipped_configobj_reference() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../../../data/configobj-reference.json")).unwrap();
        for case in reference["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let (mut values, layout) = if let Some(source) = case["source"].as_str() {
                let parsed = parse(source).unwrap_or_else(|error| panic!("{name}: {error}"));
                if let Some(expected) = case.get("parsed") {
                    assert_eq!(
                        LegacyValue::Dict(parsed.clone()),
                        LegacyValue::from(expected.clone()),
                        "{name}: parsed values"
                    );
                }
                (parsed, super::super::Layout::capture(source))
            } else {
                let LegacyValue::Dict(values) = LegacyValue::from(case["data"].clone()) else {
                    panic!("fixture dictionary")
                };
                (values, super::super::Layout::default())
            };
            if let Some(edits) = case["edit"].as_array() {
                for edit in edits {
                    let path = edit[0].as_array().unwrap();
                    let mut parent = &mut values;
                    for key in &path[..path.len() - 1] {
                        parent = parent.get_mut(key.as_str().unwrap()).unwrap().as_dict_mut().unwrap();
                    }
                    parent.insert(path.last().unwrap().as_str().unwrap(), LegacyValue::from(edit[1].clone()));
                }
            }
            let result = layout.encode(&values);
            if case["error"] == true {
                assert!(result.is_err(), "{name}: original raised");
            } else {
                assert_eq!(result.unwrap(), case["output"].as_str().unwrap(), "{name}: encoded file");
            }
        }
    }

    #[test]
    fn configobj_shapes_retain_strings_lists_sections_and_order() {
        let input = r#"root = 1
[APP]
log_level = 30
soundcard = ""
[[yaw]]
offset = -0.5
[SHORTCUTS]
"ejection_seat_btns " = ""
[PORT_FORWARDING]
ac = 127.0.0.1:3333,
none = ,
two = 'quoted, card', '#another'
"#;
        let parsed = parse(input).unwrap();
        let encoded = encode(&parsed).unwrap();
        assert_eq!(parse(&encoded).unwrap(), parsed);
        assert!(encoded.contains("ac = 127.0.0.1:3333,\n"));
        assert!(encoded.contains("none = ,\n"));
        assert!(encoded.contains("\"ejection_seat_btns \" = \"\"\n"));
    }

    #[test]
    fn quote_selection_keeps_literal_quotes_and_newlines() {
        let mut values = Dict::new();
        for (i, text) in
            ["", "a,b", "#comment", " spaces ", "a\"b", "a'b", "a'\"b", "first\nsecond", "x'''\ny", "a\n'", "a\n\""]
                .into_iter()
                .enumerate()
        {
            values.insert(format!("key{i}"), text.into());
        }
        let encoded = encode(&values).unwrap();
        assert_eq!(parse(&encoded).unwrap(), values);
        values.insert("invalid", "a'''b\"\"\"c".into());
        assert_eq!(encode(&values).unwrap_err().path, "invalid");
    }

    #[test]
    fn python_scalar_strings_and_unrepresentable_list_values() {
        let mut values = Dict::new();
        values.insert("bool", true.into());
        values.insert("none", LegacyValue::None);
        values.insert("int", 1.into());
        values.insert("float", 1.0.into());
        assert_eq!(encode(&values).unwrap(), "bool = True\nnone = None\nint = 1\nfloat = 1.0\n");
        values.insert("bad", LegacyValue::List(vec![LegacyValue::Dict(Dict::new())]));
        assert!(encode(&values).unwrap().ends_with("bad = {},\n"));
    }
}

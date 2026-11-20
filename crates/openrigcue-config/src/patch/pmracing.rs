//! Project Motor Racing `savegame*\settings.xml` UDP parameters.

use std::fs;
use std::path::{Path, PathBuf};

use super::env::{PatchEnvironment, PortKey, UserFolder, UserPath};
use super::pycodecs::{self, Encoded, Lookup};
use super::pyunicode;
use super::text::{self, is_py_space, Encoding};
use super::winpath;
use super::xml::{self, Element};
use super::PatchError;

const BASE_DIR: UserPath = UserPath::new(UserFolder::Documents, "My Games\\ProjectMotorRacing");
const SAVEGAME_PREFIX: &str = "savegame";
const SETTINGS_FILE: &str = "settings.xml";
const TABLE_CLOSE: &str = "</table";
/// The game's own (invalid) closing tag, restored on write.
const GAME_TABLE_CLOSE: &str = "</table name=\"Settings\" type=\"table\">";

/// Expected `<param>` entries: name, value, type.
fn expected_params(port: String) -> [(&'static str, String, &'static str); 3] {
    [
        ("SettingsPreferencesUDPEnabled", "2".to_owned(), "number"),
        ("SettingsPreferencesUDPPort", port, "string"),
        ("SettingsPreferencesUDPFrequency", "4".to_owned(), "number"),
    ]
}

/// Every `savegame` / `savegameN` folder's `settings.xml` (the name starts with `savegame` in any case, the
/// rest of the name empty or all digits), in the order Windows lists the folder.
fn settings_files(env: &PatchEnvironment<'_>) -> Vec<PathBuf> {
    let Some(base) = env.prefix.map(|prefix| BASE_DIR.resolve(prefix)) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries.flatten().filter_map(|entry| entry.file_name().into_string().ok()).collect();
    names.sort_by_key(|name| ntfs_order_key(name));
    names
        .into_iter()
        .filter(|name| is_savegame_folder(name))
        .map(|name| winpath::resolve(&base.join(name), SETTINGS_FILE))
        .filter(|path| path.is_file())
        .collect()
}

/// Directory listing order on NTFS: names compared by their upper-cased UTF-16 code units (the volume's `$UpCase`
/// table, taken as the simple upper-case mapping), then as they are.
fn ntfs_order_key(name: &str) -> (Vec<u16>, Vec<u16>) {
    let units: Vec<u16> = name.encode_utf16().collect();
    let upper = units
        .iter()
        .map(|&unit| {
            let Some(c) = char::from_u32(u32::from(unit)) else { return unit };
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => u16::try_from(u32::from(single)).unwrap_or(unit),
                _ => unit,
            }
        })
        .collect();
    (upper, units)
}

fn is_savegame_folder(name: &str) -> bool {
    if !name.to_lowercase().starts_with(SAVEGAME_PREFIX) {
        return false;
    }
    let suffix: String = name.chars().skip(SAVEGAME_PREFIX.chars().count()).collect();
    suffix.is_empty() || pyunicode::str_is_digit(&suffix)
}

/// At least one settings file exists.
pub fn installed(env: &PatchEnvironment<'_>) -> bool {
    !settings_files(env).is_empty()
}

/// Every settings file parses and has the three parameters with matching value and type.
pub fn patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let files = settings_files(env);
    if files.is_empty() {
        return Ok(false);
    }
    let expected = expected_params(env.port_text(PortKey::PmRacing)?);
    for path in files {
        let Ok(document) = load(&path) else { return Ok(false) };
        if !has_params(&document.root, &expected) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Adds or corrects the three parameters in every parsable settings file and writes the changed ones back,
/// keeping the file's own declaration and closing tag; no backup. `Ok(false)` when no file is found; a failed
/// write is an error.
pub fn patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let files = settings_files(env);
    if files.is_empty() {
        return Ok(false);
    }
    let expected = expected_params(env.port_text(PortKey::PmRacing)?);
    for path in files {
        let Ok(mut document) = load(&path) else { continue };
        if set_params(&mut document.root, &expected) {
            save(&path, &document)?;
        }
    }
    Ok(true)
}

struct SettingsDocument {
    root: Element,
    declaration: Option<String>,
    had_table_close: bool,
}

/// Reads as UTF-8, splits off the declaration and normalizes every `</table…>` to `</table>` before parsing.
fn load(path: &Path) -> Result<SettingsDocument, PatchError> {
    let raw = text::read_text(path, Encoding::Utf8)?;
    let (declaration, body) = split_declaration(&raw);
    let (fixed, had_table_close) = normalize_table_close(body);
    Ok(SettingsDocument { root: xml::parse_str(&fixed)?, declaration: declaration.map(str::to_owned), had_table_close })
}

/// Splits leading whitespace and an `<?xml … ?>` declaration from the rest of the text.
fn split_declaration(raw: &str) -> (Option<&str>, &str) {
    let trimmed = raw.trim_start_matches(is_py_space);
    if trimmed.starts_with("<?xml") {
        if let Some(end) = trimmed.find('>').filter(|end| trimmed[..*end].ends_with('?')) {
            return (Some(&trimmed[..=end]), &trimmed[end + 1..]);
        }
    }
    (None, raw)
}

/// Replaces every `</table…>` (any case) with `</table>`; also returns whether anything matched.
fn normalize_table_close(body: &str) -> (String, bool) {
    let mut out = String::new();
    let mut rest = body;
    let mut matched = false;
    while let Some(start) = find_ascii_insensitive(rest, TABLE_CLOSE) {
        let Some(close) = rest[start..].find('>') else { break };
        out.push_str(&rest[..start]);
        out.push_str("</table>");
        rest = &rest[start + close + 1..];
        matched = true;
    }
    out.push_str(rest);
    (out, matched)
}

fn find_ascii_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .char_indices()
        .map(|(index, _)| index)
        .find(|&index| haystack.get(index..index + needle.len()).is_some_and(|s| s.eq_ignore_ascii_case(needle)))
}

/// Serializes without pretty-printing, restores the game's closing tag and declaration, writes with `\n` line
/// ends in the declared encoding: the file is created or
/// emptied before the codec is looked up, and the whole text is encoded at once, so an unknown
/// or binary codec, or a character the codec lacks, leaves the file empty and fails.
fn save(path: &Path, document: &SettingsDocument) -> Result<(), PatchError> {
    let encoding_name = document.declaration.as_deref().and_then(declared_encoding).unwrap_or("utf-8".to_owned());
    let mut body = xml::to_string(&document.root);
    if document.had_table_close {
        body = restore_table_close(&body);
    }
    let content = match &document.declaration {
        Some(declaration) => format!("{declaration}\n{body}"),
        None => format!("<?xml version=\"1.0\" encoding=\"{encoding_name}\"?>\n{body}"),
    };
    let encoded = match pycodecs::lookup(&encoding_name) {
        Lookup::Found { module, text: true } => pycodecs::encode(module, &content),
        Lookup::Found { text: false, .. } | Lookup::Unknown => {
            fs::write(path, b"")?;
            return Err(PatchError::UnsupportedEncoding(encoding_name));
        }
    };
    match encoded {
        Encoded::Bytes(bytes) => fs::write(path, bytes)?,
        Encoded::Unencodable => {
            fs::write(path, b"")?;
            return Err(PatchError::Unencodable(encoding_name));
        }
    }
    Ok(())
}

/// Replaces a final `</table>` (and trailing whitespace) with [`GAME_TABLE_CLOSE`].
fn restore_table_close(body: &str) -> String {
    let start = body
        .match_indices("</table>")
        .map(|(index, _)| index)
        .find(|&index| body[index + "</table>".len()..].chars().all(is_py_space));
    match start {
        Some(index) => format!("{}{GAME_TABLE_CLOSE}", &body[..index]),
        None => body.to_owned(),
    }
}

/// The declared encoding: the first `encoding=` (any case; the `i`
/// also matches `İ` and `ı`) followed by a quoted value.
fn declared_encoding(declaration: &str) -> Option<String> {
    let chars: Vec<char> = declaration.chars().collect();
    let keyword: Vec<char> = "encoding=".chars().collect();
    let matches_at = |start: usize| {
        keyword.iter().enumerate().all(|(offset, expected)| {
            chars.get(start + offset).is_some_and(|c| match expected {
                'i' => matches!(c, 'i' | 'I' | '\u{130}' | '\u{131}'),
                other => c.eq_ignore_ascii_case(other),
            })
        })
    };
    (0..chars.len()).filter(|start| matches_at(*start)).find_map(|start| {
        let open = start + keyword.len();
        if !matches!(chars.get(open), Some('"' | '\'')) {
            return None;
        }
        let value_start = open + 1;
        let end = (value_start..chars.len()).find(|index| matches!(chars[*index], '"' | '\''))?;
        (end > value_start).then(|| chars[value_start..end].iter().collect())
    })
}

/// `<param>` children of the first `parameters` descendant, by name (last wins); `None` when there is none.
fn params_by_name(parameters: &Element) -> Vec<(&str, usize)> {
    let mut found: Vec<(&str, usize)> = Vec::new();
    for (index, param) in parameters.children.iter().enumerate().filter(|(_, child)| child.tag == "param") {
        let Some(name) = param.attribute("name").filter(|name| !name.is_empty()) else {
            continue;
        };
        found.retain(|(existing, _)| *existing != name);
        found.push((name, index));
    }
    found
}

fn has_params(root: &Element, expected: &[(&str, String, &str)]) -> bool {
    let empty = Element::default();
    let parameters = match root.find_descendant_path("parameters") {
        Some(path) => path.iter().fold(root, |element, index| &element.children[*index]),
        None => &empty,
    };
    let by_name = params_by_name(parameters);
    expected.iter().all(|(name, value, kind)| {
        by_name.iter().find(|(found, _)| found == name).is_some_and(|(_, index)| {
            let param = &parameters.children[*index];
            param.attribute("type") == Some(*kind) && param.attribute("value") == Some(value.as_str())
        })
    })
}

/// Adds missing parameters (attributes name, type, value) and corrects type then value; true when changed.
fn set_params(root: &mut Element, expected: &[(&str, String, &str)]) -> bool {
    let parameters = match root.find_descendant_path("parameters") {
        Some(path) => root.at_path_mut(&path),
        None => {
            root.children.push(Element::new("parameters", Vec::new()));
            root.children.last_mut().expect("just pushed")
        }
    };
    let by_name: Vec<(String, usize)> =
        params_by_name(parameters).into_iter().map(|(name, index)| (name.to_owned(), index)).collect();
    let mut changed = false;
    for (name, value, kind) in expected {
        match by_name.iter().find(|(found, _)| found == name) {
            None => {
                let attributes = vec![
                    ("name".to_owned(), name.to_string()),
                    ("type".to_owned(), kind.to_string()),
                    ("value".to_owned(), value.clone()),
                ];
                parameters.children.push(Element::new("param", attributes));
                changed = true;
            }
            Some((_, index)) => {
                let param = &mut parameters.children[*index];
                for (attribute, wanted) in [("type", *kind), ("value", value.as_str())] {
                    if param.attribute(attribute) != Some(wanted) {
                        param.set_attribute(attribute, wanted);
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::testing::{ports, prefix_fixture};

    const SETTINGS: &str = "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\r\n<table name=\"Settings\" type=\"table\">\r\n  <parameters>\r\n    <param name=\"SettingsPreferencesUDPEnabled\" type=\"number\" value=\"0\"/>\r\n  </parameters>\r\n</table name=\"Settings\" type=\"table\">\r\n";

    #[test]
    fn native_live_port_text_preserves_pmracing_values_and_lookup_order() {
        let prefix = prefix_fixture("pmr-native-port");
        let folder = prefix.documents().join("My Games/ProjectMotorRacing/savegame");
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("settings.xml");
        for raw in ["001337", "+0070000", "-1", "\" 001337 \""] {
            let parsed = crate::import::configobj::parse(&format!("[NETWORK_PORT]\npmracing = {raw}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("pmracing").unwrap().clone();
            let wanted = value.py_str().unwrap();
            let mut settings = crate::settings::Settings::default();
            settings.edit_native(|config| {
                config.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("pmracing", value);
            });
            let loaded = ModulePorts::at_startup(&settings);
            let env = PatchEnvironment {
                prefix: Some(&prefix),
                steam: None,
                library: &crate::patch::library::BUILTIN,
                ports: &settings,
                loaded_ports: &loaded,
            };
            fs::write(&file, SETTINGS).unwrap();
            assert!(patch(&env).unwrap());
            let doc = load(&file).unwrap();
            let path = doc.root.find_descendant_path("parameters").unwrap();
            let parameters = path.iter().fold(&doc.root, |element, index| &element.children[*index]);
            let port = parameters
                .children
                .iter()
                .find(|param| param.attribute("name") == Some("SettingsPreferencesUDPPort"))
                .unwrap();
            assert_eq!(port.attribute("value"), Some(wanted.as_str()), "{raw}");
            assert!(patched(&env).unwrap());
        }
        let missing = |_: PortKey| None;
        let loaded = ModulePorts::at_startup(&missing);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &missing,
            loaded_ports: &loaded,
        };
        fs::write(&file, b"not XML").unwrap();
        assert!(matches!(patch(&env), Err(PatchError::MissingPort(PortKey::PmRacing))));
        assert_eq!(fs::read(&file).unwrap(), b"not XML");
        fs::remove_file(&file).unwrap();
        assert!(!patch(&env).unwrap());
        assert!(!patched(&env).unwrap());
    }

    /// Patch: parameters corrected/added inline, declaration and quirky closing tag restored, LF.
    #[test]
    fn settings_round_trip() {
        let prefix = prefix_fixture("pmr");
        let base = prefix.documents().join("My Games/ProjectMotorRacing");
        for folder in ["savegame", "SaveGame2", "savegame_old"] {
            fs::create_dir_all(base.join(folder)).unwrap();
            fs::write(base.join(folder).join("settings.xml"), SETTINGS).unwrap();
        }
        let lookup = ports(5555);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert_eq!(settings_files(&env).len(), 2);
        assert!(installed(&env));
        assert!(!patched(&env).unwrap());
        patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(base.join("savegame/settings.xml")).unwrap(),
            "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<table name=\"Settings\" type=\"table\">\n  \
<parameters>\n    <param name=\"SettingsPreferencesUDPEnabled\" type=\"number\" value=\"2\" />\n  \
<param name=\"SettingsPreferencesUDPPort\" type=\"string\" value=\"5555\" /><param \
name=\"SettingsPreferencesUDPFrequency\" type=\"number\" value=\"4\" /></parameters>\n</table name=\"Settings\" type=\"table\">"
        );
        assert!(patched(&env).unwrap());
        assert_eq!(fs::read_to_string(base.join("savegame_old/settings.xml")).unwrap(), SETTINGS);
    }

    /// Saving: the declared encoding writes the file (cp1252 here); an unknown codec or a character
    /// the codec lacks empties the file and fails the patch.
    #[test]
    fn declared_encodings() {
        let prefix = prefix_fixture("pmr-enc");
        let folder = prefix.documents().join("My Games/ProjectMotorRacing/savegame");
        fs::create_dir_all(&folder).unwrap();
        let file = folder.join("settings.xml");
        let settings = |encoding: &str, text: &str| {
            format!("<?xml version=\"1.0\" encoding=\"{encoding}\"?>\n<table><parameters/><x>{text}</x></table>")
        };
        let lookup = ports(5555);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        fs::write(&file, settings("Windows-1252", "\u{e9}")).unwrap();
        patch(&env).unwrap();
        let written = fs::read(&file).unwrap();
        assert!(written.windows(8).any(|w| w == b"<x>\xe9</x>"));
        fs::write(&file, settings("no-such-codec", "x")).unwrap();
        assert!(matches!(patch(&env), Err(PatchError::UnsupportedEncoding(_))));
        assert_eq!(fs::read(&file).unwrap(), b"");
        fs::write(&file, settings("ascii", "\u{e9}")).unwrap();
        assert!(matches!(patch(&env), Err(PatchError::Unencodable(_))));
        assert_eq!(fs::read(&file).unwrap(), b"");
        fs::write(&file, settings("utf-16", "x")).unwrap();
        patch(&env).unwrap();
        assert!(fs::read(&file).unwrap().starts_with(&[0xFF, 0xFE, b'<', 0]));
    }

    /// Patched check: no settings file is false before the port is read; with a file,
    /// the missing port is an error.
    #[test]
    fn missing_files_checked_before_port() {
        let prefix = prefix_fixture("pmr-noport");
        let lookup = |_: PortKey| None;
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(!patched(&env).unwrap());
        let folder = prefix.documents().join("My Games/ProjectMotorRacing/savegame");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("settings.xml"), SETTINGS).unwrap();
        assert!(matches!(patched(&env), Err(PatchError::MissingPort(PortKey::PmRacing))));
    }

    /// The text-matching helpers.
    #[test]
    fn regex_helpers() {
        assert_eq!(split_declaration("  <?xml a?>rest"), (Some("<?xml a?>"), "rest"));
        assert_eq!(split_declaration("<?xml a>rest"), (None, "<?xml a>rest"));
        assert_eq!(normalize_table_close("<a></TABLE x></table>"), ("<a></table></table>".to_owned(), true));
        assert_eq!(restore_table_close("<t></t></table>\n "), "<t></t></table name=\"Settings\" type=\"table\">");
        assert_eq!(declared_encoding("<?xml ENCODING='UTF-8'?>"), Some("UTF-8".to_owned()));
        assert_eq!(declared_encoding("<?xml encoding = 'x'?>"), None);
        assert_eq!(declared_encoding("<?xml encoding=x ENCODİNG=\"cp1252\"?>"), Some("cp1252".to_owned()));
        assert_eq!(declared_encoding("<?xml encoding='a\"?>"), Some("a".to_owned()));
        assert!(
            is_savegame_folder("SAVEGAME")
                && is_savegame_folder("savegame\u{b2}")
                && is_savegame_folder("SaveGame\u{0663}")
        );
        assert!(!is_savegame_folder("savegame_1") && !is_savegame_folder("save"));
        let mut names = vec!["savegame10", "SaveGame2", "savegame1", "b", "A"];
        names.sort_by_key(|name| ntfs_order_key(name));
        assert_eq!(names, ["A", "b", "savegame1", "savegame10", "SaveGame2"]);
    }
}

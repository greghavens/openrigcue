//! Wreckfest 2 `savegame\telemetry\config.json` UDP outputs.

use std::fs;
use std::path::{Path, PathBuf};

use super::env::{PatchEnvironment, PortKey, UserFolder, UserPath};
use super::pyjson::{self, Dict, DumpStyle, PyValue};
use super::pyunicode::PyInt;
use super::text::{self, Encoding, Newline};
use super::winpath;
use super::PatchError;

/// Backup next to `config.json`.
const BACKUP_FILE: &str = "config_srs_bck.json";
const GAME_DIR: UserPath = UserPath::new(UserFolder::Documents, "My Games\\Wreckfest 2");

/// `<game>\<first sub-folder in sorted order>\savegame\telemetry\config.json`.
fn config_path(env: &PatchEnvironment<'_>) -> Option<PathBuf> {
    let base = GAME_DIR.resolve(env.prefix?);
    let mut folders: Vec<String> = fs::read_dir(&base)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    folders.sort();
    let profile = folders.into_iter().next().filter(|name| !name.is_empty())?;
    Some(winpath::resolve(&base.join(profile), "savegame\\telemetry\\config.json"))
}

fn read_config(path: &Path) -> Option<PyValue> {
    pyjson::parse(&text::read_text(path, Encoding::Utf8).ok()?).ok()
}

/// `config.json` exists.
pub fn installed(env: &PatchEnvironment<'_>) -> bool {
    config_path(env).is_some_and(|path| path.is_file())
}

/// `udp` is non-empty and every object entry has `enabled` equal to 1 as an integer and `port` equal to the
/// port as text; any failure is false, including a missing `config.json` (checked before the port is read)
/// and a missing `NETWORK_PORT.wreckfest2`.
pub fn patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let Some(PyValue::Dict(data)) = config_path(env).filter(|path| path.is_file()).and_then(|path| read_config(&path))
    else {
        return Ok(false);
    };
    let Ok(port) = env.port_text(PortKey::Wreckfest2) else {
        return Ok(false);
    };
    let entries: Vec<&Dict> = match data.get("udp") {
        Some(PyValue::List(items)) if !items.is_empty() => items.iter().filter_map(PyValue::as_dict).collect(),
        // A non-empty object or string yields keys or characters, none of which is an entry to check.
        Some(PyValue::Dict(map)) if !map.is_empty() => Vec::new(),
        Some(PyValue::Str(text)) if !text.is_empty() => Vec::new(),
        // Missing, empty or not iterable.
        _ => return Ok(false),
    };
    Ok(entries.into_iter().all(|entry| {
        let enabled = entry.get("enabled").map_or(Some(PyInt::Small(0)), pyjson::py_int);
        let port_matches = entry.get("port").is_some_and(|value| pyjson::py_str_is(value, &port));
        enabled == Some(PyInt::Small(1)) && port_matches
    }))
}

/// Sets `enabled = 1` and the port (as a string) on every object entry of `udp`, or replaces a missing/empty
/// `udp` by one entry; backs the file up and writes it with indent 2, without ASCII escaping, plus a newline.
///
/// `Ok(false)` without the file, when it does not parse, or when the backup copy or the write fails; a
/// configuration that is not an object is an error.
pub fn patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let Some(path) = config_path(env).filter(|path| path.is_file()) else { return Ok(false) };
    let Some(data) = read_config(&path) else { return Ok(false) };
    let port = env.port_text(PortKey::Wreckfest2)?;
    let PyValue::Dict(mut data) = data else {
        return Err(PatchError::UnexpectedJson);
    };
    match data.get_mut("udp") {
        Some(PyValue::List(entries)) if !entries.is_empty() => {
            for entry in entries.iter_mut().filter_map(PyValue::as_dict_mut) {
                entry.insert("enabled", PyValue::int(1));
                entry.insert("port", PyValue::str(&port));
            }
        }
        _ => {
            let entry = Dict::of([
                ("enabled", PyValue::int(1)),
                ("ip", PyValue::str("127.0.0.1")),
                ("port", PyValue::str(&port)),
            ]);
            data.insert("udp", PyValue::List(vec![PyValue::Dict(entry)]));
        }
    }
    let backup = winpath::resolve(path.parent().unwrap_or(Path::new("")), BACKUP_FILE);
    if backup.is_file() {
        let _ = fs::remove_file(&backup);
    }
    if fs::copy(&path, &backup).is_err() {
        return Ok(false);
    }
    // The JSON is written chunk by chunk (a lone surrogate fails its chunk, leaving the ones before), then
    // a final newline.
    let style = DumpStyle { ensure_ascii: false, ..DumpStyle::indented(2) };
    let chunks = pyjson::dump_chunks(&PyValue::Dict(data), &style).into_iter().map(|chunk| chunk.to_text());
    Ok(text::write_chunks(&path, chunks.chain([Some("\n".to_owned())]), Encoding::Utf8, Newline::Windows).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::testing::{ports, prefix_fixture};

    fn setup(tag: &str, content: &str) -> (crate::patch::prefix::WinePrefix, PathBuf) {
        let prefix = prefix_fixture(tag);
        let telemetry = prefix.documents().join("My Games/Wreckfest 2/76561190000000001/savegame/telemetry");
        fs::create_dir_all(&telemetry).unwrap();
        fs::create_dir_all(prefix.documents().join("My Games/Wreckfest 2/9-later")).unwrap();
        fs::write(telemetry.join("config.json"), content).unwrap();
        (prefix, telemetry.join("config.json"))
    }

    #[test]
    fn native_live_port_text_preserves_wreckfest_values_and_read_backup_order() {
        let original = r#"{"udp":[{"enabled":0,"port":"other"}]}"#;
        let (prefix, file) = setup("wf2-native-port", original);
        for raw in ["001337", "+0070000", "-1", "\" 001337 \""] {
            let parsed = crate::import::configobj::parse(&format!("[NETWORK_PORT]\nwreckfest2 = {raw}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("wreckfest2").unwrap().clone();
            let wanted = value.py_str().unwrap();
            let mut settings = crate::settings::Settings::default();
            settings.edit_native(|config| {
                config.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("wreckfest2", value);
            });
            let loaded = ModulePorts::at_startup(&settings);
            let env = PatchEnvironment {
                prefix: Some(&prefix),
                steam: None,
                library: &crate::patch::library::BUILTIN,
                ports: &settings,
                loaded_ports: &loaded,
            };
            fs::write(&file, original).unwrap();
            assert!(patch(&env).unwrap());
            let data = read_config(&file).unwrap();
            let PyValue::List(entries) = data.get("udp").unwrap() else { panic!("list") };
            assert!(pyjson::py_str_is(entries[0].get("port").unwrap(), &wanted), "{raw}");
            assert_eq!(fs::read_to_string(file.with_file_name(BACKUP_FILE)).unwrap(), original);
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
        fs::write(&file, "bad JSON").unwrap();
        assert!(!patch(&env).unwrap());
        fs::write(&file, original).unwrap();
        assert!(matches!(patch(&env), Err(PatchError::MissingPort(PortKey::Wreckfest2))));
        assert!(!patched(&env).unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), original);
    }

    /// Patch: every object entry enabled with the string port, other entries untouched, backup, '\n' end.
    #[test]
    fn udp_entries() {
        let (prefix, path) = setup("wf2", r#"{"udp":[{"enabled":0,"ip":"10.0.0.2","port":1},"x"],"name":"é"}"#);
        let lookup = ports(5606);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(installed(&env));
        assert!(!patched(&env).unwrap());
        patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\r\n  \"udp\": [\r\n    {\r\n      \"enabled\": 1,\r\n      \"ip\": \"10.0.0.2\",\r\n      \"port\": \"5606\"\r\n    },\r\n    \
\"x\"\r\n  ],\r\n  \"name\": \"é\"\r\n}\r\n"
        );
        assert!(path.with_file_name("config_srs_bck.json").is_file());
        assert!(patched(&env).unwrap());
    }

    /// Patched check: the integer `enabled` and text `port` rules; patch replaces an empty udp list.
    #[test]
    fn patched_rules_and_empty_list() {
        let (prefix, path) = setup("wf2-rules", r#"{"udp":[{"enabled":"1","port":5606}]}"#);
        let lookup = ports(5606);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(patched(&env).unwrap());
        fs::write(&path, r#"{"udp":[{"enabled":1.5,"port":"5606"},{"enabled":"yes","port":"5606"}]}"#).unwrap();
        assert!(!patched(&env).unwrap());
        fs::write(&path, r#"{"udp":[],"x":1}"#).unwrap();
        assert!(!patched(&env).unwrap());
        patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\r\n  \"udp\": [\r\n    {\r\n      \"enabled\": 1,\r\n      \"ip\": \"127.0.0.1\",\r\n      \
\"port\": \"5606\"\r\n    }\r\n  ],\r\n  \"x\": 1\r\n}\r\n"
        );
    }

    /// Patch: a lone surrogate in a string fails its output chunk; the chunks before it stay in the
    /// (truncated) file, the final newline is not written, and the patch is `Ok(false)`.
    #[test]
    fn lone_surrogate_leaves_a_partial_file() {
        let (prefix, path) = setup("wf2-surrogate", r#"{"name":"\ud800","udp":[]}"#);
        let lookup = ports(5606);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(!patch(&env).unwrap(), "the write is inside the original's try: False");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\r\n  \"name\": ");
        assert!(path.with_file_name("config_srs_bck.json").is_file());
    }

    /// Patched check: a missing file is false before the port is read, and a missing
    /// NETWORK_PORT.wreckfest2 is false too.
    #[test]
    fn missing_port_or_file_is_not_patched() {
        let (prefix, path) = setup("wf2-noport", r#"{"udp":[{"enabled":1,"port":"5606"}]}"#);
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
        fs::remove_file(&path).unwrap();
        assert!(!patched(&env).unwrap());
    }
}

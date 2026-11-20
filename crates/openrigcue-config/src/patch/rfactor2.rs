//! rFactor 2 and Le Mans Ultimate: the shared-memory plugin DLL plus its entry in
//! `UserData\player\CustomPluginVariables.JSON`.

use std::io;
use std::path::PathBuf;

use super::env::PatchEnvironment;
use super::pyjson::{self, Dict, DumpStyle, PyValue};
use super::steam::SteamPlugin;
use super::text::{self, Encoding, Newline};
use super::winpath;
use super::PatchError;

/// Key of the plugin in `CustomPluginVariables.JSON`.
const PLUGIN_KEY: &str = "rFactor2SharedMemoryMapPlugin64.dll";
/// Library copy written when Le Mans Ultimate has no `CustomPluginVariables.JSON` yet.
const DEFAULT_VARIABLES: &str = "CustomPluginVariables.JSON";
const VARIABLES_RELATIVE: &str = "UserData\\player\\CustomPluginVariables.JSON";

pub const RFACTOR2_PLUGIN: SteamPlugin =
    SteamPlugin { location: "rFactor 2\\Bin64\\Plugins\\rFactor2SharedMemoryMapPlugin64.dll" };
pub const LE_MANS_PLUGIN: SteamPlugin =
    SteamPlugin { location: "Le Mans Ultimate\\Plugins\\rFactor2SharedMemoryMapPlugin64.dll" };
/// Steam location used to find Le Mans Ultimate's `UserData` folder.
const LE_MANS_VARIABLES: &str = "Le Mans Ultimate\\UserData\\player\\CustomPluginVariables.JSON";

/// The plugin entry (the leading space of `" Enabled"` is rFactor 2's own spelling).
const PLUGIN_ENTRY: [(&str, i128); 5] = [
    (" Enabled", 1),
    ("DebugISIInternals", 0),
    ("DebugOutputLevel", 0),
    ("DedicatedServerMapGlobally", 0),
    ("EnableStockCarRulesPlugin", 0),
];

fn plugin_entry() -> Dict {
    Dict::of(PLUGIN_ENTRY.map(|(key, value)| (key, PyValue::int(value))))
}

/// `<library>\rFactor 2\UserData\player\CustomPluginVariables.JSON`.
fn rfactor2_variables(env: &PatchEnvironment<'_>) -> Option<PathBuf> {
    let bin = env.steam?.find_game(RFACTOR2_PLUGIN.location)?;
    Some(winpath::resolve(bin.parent()?, VARIABLES_RELATIVE))
}

/// `<library>\Le Mans Ultimate\UserData` + `\player\CustomPluginVariables.JSON`.
fn le_mans_variables(env: &PatchEnvironment<'_>) -> Option<PathBuf> {
    let user_data = env.steam?.find_game(LE_MANS_VARIABLES)?;
    Some(winpath::resolve(&user_data, "player\\CustomPluginVariables.JSON"))
}

fn object(value: PyValue) -> Result<Dict, PatchError> {
    match value {
        PyValue::Dict(map) => Ok(map),
        _ => Err(PatchError::UnexpectedJson),
    }
}

/// Writes the JSON output chunk by chunk.
fn write_json(path: &std::path::Path, data: Dict, style: &DumpStyle, encoding: Encoding) -> io::Result<()> {
    let chunks = pyjson::dump_chunks(&PyValue::Dict(data), style).into_iter().map(|chunk| chunk.to_text());
    text::write_chunks(path, chunks, encoding, Newline::Windows)
}

/// rFactor 2: plugin copied and every entry field equal (loose equality); a missing or unparsable file is false.
pub fn rfactor2_patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let Some(steam) = env.steam else { return Ok(false) };
    if !RFACTOR2_PLUGIN.patched(steam, env.library) {
        return Ok(false);
    }
    let Some(path) = rfactor2_variables(env).filter(|path| path.is_file()) else {
        return Ok(false);
    };
    let Some(data) = text::read_text(&path, Encoding::Utf8Sig).ok().and_then(|t| pyjson::parse(&t).ok()) else {
        return Ok(false);
    };
    let data = object(data)?;
    let entry = match data.get(PLUGIN_KEY) {
        None | Some(PyValue::Null) => return Ok(false),
        Some(PyValue::Dict(entry)) => entry,
        Some(_) => return Err(PatchError::UnexpectedJson),
    };
    Ok(PLUGIN_ENTRY
        .iter()
        .all(|(key, value)| entry.get(key).is_some_and(|found| pyjson::py_eq(found, &PyValue::int(*value)))))
}

/// rFactor 2: copies the plugin, then merges the entry fields into the existing entry (an unreadable file
/// starts empty) and writes the file with indent 2 and compact separators; no backup.
///
/// `Ok(false)` when copying the plugin fails, the file's path cannot be resolved or the write fails.
pub fn rfactor2_patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let steam = env.steam.ok_or(PatchError::NotInstalled)?;
    if !RFACTOR2_PLUGIN.patch(steam, env.library, env.drive_root().as_deref())? {
        return Ok(false);
    }
    let Some(path) = rfactor2_variables(env) else { return Ok(false) };
    let parsed = text::read_text(&path, Encoding::Utf8Sig).ok().and_then(|t| pyjson::parse(&t).ok());
    let mut data = object(parsed.unwrap_or(PyValue::Dict(Dict::new())))?;
    if data.get(PLUGIN_KEY).is_none() {
        data.insert(PLUGIN_KEY, PyValue::Dict(Dict::new()));
    }
    let Some(PyValue::Dict(entry)) = data.get_mut(PLUGIN_KEY) else {
        return Err(PatchError::UnexpectedJson);
    };
    entry.update(plugin_entry());
    let style = DumpStyle { indent: 2, item_separator: ",", key_separator: ":", ensure_ascii: true };
    Ok(write_json(&path, data, &style, Encoding::Utf8).is_ok())
}

/// Le Mans Ultimate: plugin copied and the file has the key with exactly the entry (loose equality, any key order).
pub fn le_mans_patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let Some(steam) = env.steam else { return Ok(false) };
    if !LE_MANS_PLUGIN.patched(steam, env.library) {
        return Ok(false);
    }
    let Some(path) = le_mans_variables(env).filter(|path| path.is_file()) else {
        return Ok(false);
    };
    let Some(data) = text::read_text(&path, Encoding::Ansi).ok().and_then(|t| pyjson::parse(&t).ok()) else {
        return Ok(false);
    };
    // A file holding `null` is not patched; any other non-object is an error.
    if data == PyValue::Null {
        return Ok(false);
    }
    let expected = PyValue::Dict(plugin_entry());
    Ok(object(data)?.get(PLUGIN_KEY).is_some_and(|entry| pyjson::py_eq(entry, &expected)))
}

/// Le Mans Ultimate: copies the plugin, then sets the entry in an existing file (indent 4) or copies the
/// library's default file; no backup.
///
/// `Ok(true)` also when copying the plugin fails and when the game is not found;
/// `Ok(false)` only when copying the default file or reading the existing one fails. A file
/// that is no JSON object is an error, as is a failed write other than a missing path
/// (printed).
pub fn le_mans_patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let steam = env.steam.ok_or(PatchError::NotInstalled)?;
    if !LE_MANS_PLUGIN.patch(steam, env.library, env.drive_root().as_deref())? {
        return Ok(true);
    }
    let Some(path) = le_mans_variables(env) else { return Ok(true) };
    if !path.is_file() {
        return Ok(env.library.copy_to(DEFAULT_VARIABLES, &path).is_ok());
    }
    // A missing file is printed and reads as null, which the assignment then fails on.
    let data = match text::read_text(&path, Encoding::Ansi) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(PatchError::UnexpectedJson),
        Err(_) => return Ok(false),
        Ok(text) => match pyjson::parse(&text) {
            Ok(data) => data,
            Err(_) => return Ok(false),
        },
    };
    let mut data = object(data)?;
    data.insert(PLUGIN_KEY, PyValue::Dict(plugin_entry()));
    match write_json(&path, data, &DumpStyle::indented(4), Encoding::Ansi) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(path = %path.display(), "error writing CustomPluginVariables.JSON");
            Ok(true)
        }
        other => other.map(|()| true).map_err(PatchError::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::steam::SteamLibraries;
    use crate::patch::testing::{ports, scratch};
    use std::fs;

    fn fixture(tag: &str, game: &str) -> (PathBuf, SteamLibraries, crate::patch::library::Library) {
        let base = scratch(tag);
        let common = base.join("Steam/steamapps/common");
        fs::create_dir_all(common.join(game)).unwrap();
        let library = base.join("libraries");
        fs::create_dir_all(&library).unwrap();
        fs::write(library.join(PLUGIN_KEY), b"MZ").unwrap();
        fs::write(library.join(DEFAULT_VARIABLES), "{\n  \"default\": 1\n}").unwrap();
        (common, SteamLibraries::of(&base.join("Steam")), crate::patch::library::Library::Folder(library))
    }

    /// rFactor 2 patch: existing entry keys updated in place, new keys appended, compact separators, CRLF.
    #[test]
    fn rfactor2_merge() {
        let (common, steam, library) = fixture("rf2", "rFactor 2/Bin64/Plugins");
        let player = common.join("rFactor 2/UserData/player");
        fs::create_dir_all(&player).unwrap();
        fs::write(
            player.join("CustomPluginVariables.JSON"),
            "\u{feff}{\"Other.dll\":{\" Enabled\":0},\"rFactor2SharedMemoryMapPlugin64.dll\":{\"DebugOutputLevel\":5,\"Extra\":\"é\"}}",
        )
        .unwrap();
        let lookup = ports(0);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: None,
            steam: Some(&steam),
            library: &library,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(!rfactor2_patched(&env).unwrap());
        rfactor2_patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(player.join("CustomPluginVariables.JSON")).unwrap(),
            "{\r\n  \"Other.dll\":{\r\n    \" Enabled\":0\r\n  },\r\n  \"rFactor2SharedMemoryMapPlugin64.dll\":{\r\n    \
\"DebugOutputLevel\":0,\r\n    \"Extra\":\"\\u00e9\",\r\n    \" Enabled\":1,\r\n    \"DebugISIInternals\":0,\r\n    \
\"DedicatedServerMapGlobally\":0,\r\n    \"EnableStockCarRulesPlugin\":0\r\n  }\r\n}"
        );
        assert!(rfactor2_patched(&env).unwrap());
    }

    /// Le Mans Ultimate patch: missing file → library default copied; existing file → entry replaced, indent 4.
    #[test]
    fn le_mans_entry() {
        let (common, steam, library) = fixture("lmu", "Le Mans Ultimate/UserData/player");
        let lookup = ports(0);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: None,
            steam: Some(&steam),
            library: &library,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        let file = common.join("Le Mans Ultimate/UserData/player/CustomPluginVariables.JSON");
        le_mans_patch(&env).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\n  \"default\": 1\n}");
        assert!(!le_mans_patched(&env).unwrap());
        fs::write(&file, "{\"rFactor2SharedMemoryMapPlugin64.dll\": {\" Enabled\": 0, \"X\": 1}, \"b\": true}")
            .unwrap();
        le_mans_patch(&env).unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "{\r\n    \"rFactor2SharedMemoryMapPlugin64.dll\": {\r\n        \" Enabled\": 1,\r\n        \
\"DebugISIInternals\": 0,\r\n        \"DebugOutputLevel\": 0,\r\n        \"DedicatedServerMapGlobally\": 0,\r\n        \
\"EnableStockCarRulesPlugin\": 0\r\n    },\r\n    \"b\": true\r\n}"
        );
        assert!(le_mans_patched(&env).unwrap());
        assert!(common.join("Le Mans Ultimate/Plugins/rFactor2SharedMemoryMapPlugin64.dll").is_file());
    }
}

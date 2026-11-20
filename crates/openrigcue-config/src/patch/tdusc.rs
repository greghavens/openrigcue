//! Test Drive Unlimited Solar Crown `UserSettings.cfg` telemetry keys.

use std::fs;
use std::path::{Path, PathBuf};

use super::env::{PatchEnvironment, PortKey, UserFolder, UserPath};
use super::text::{self, py_strip, Encoding, Newline};
use super::PatchError;

const CONFIG: UserPath =
    UserPath::new(UserFolder::Documents, "My Games\\Test Drive Unlimited Solar Crown\\UserSettings.cfg");

/// Target settings in written order (`TelemetryAdress` is the game's spelling).
fn target_settings(port: String) -> [(&'static str, String); 4] {
    [
        ("Rc2.Telemetry.TelemetryPort", port),
        ("Rc2.Telemetry.TelemetryAdress", "\"127.0.0.1\"".to_owned()),
        ("Rc2.Telemetry.EnableTelemetry", "true".to_owned()),
        ("Rc2.Telemetry.TelemetryRate", "60".to_owned()),
    ]
}

fn config(env: &PatchEnvironment<'_>) -> Option<PathBuf> {
    env.prefix.map(|prefix| CONFIG.resolve(prefix))
}

/// UTF-16 when the whole file decodes as UTF-16 (byte-order mark, else little-endian), otherwise UTF-8.
fn detect_encoding(path: &Path) -> Result<Encoding, PatchError> {
    let bytes = fs::read(path)?;
    Ok(if text::decode(&bytes, Encoding::Utf16).is_some() { Encoding::Utf16 } else { Encoding::Utf8 })
}

/// The file exists.
pub fn installed(env: &PatchEnvironment<'_>) -> bool {
    config(env).is_some_and(|path| path.is_file())
}

/// Every key's value (trimmed, trailing `;` removed) equals its target; a read failure is false.
pub fn patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let Some(path) = config(env).filter(|path| path.is_file()) else {
        return Ok(false);
    };
    let targets = target_settings(env.port_text(PortKey::Tdusc)?);
    let encoding = detect_encoding(&path)?;
    let Ok(content) = text::read_text(&path, encoding) else {
        return Ok(false);
    };
    let mut current: Vec<(&str, &str)> = Vec::new();
    for line in text::lines_with_ends(&content).into_iter().filter(|line| line.contains('=')) {
        let mut parts = line.split('=');
        let key = py_strip(parts.next().unwrap_or_default());
        if targets.iter().any(|(target, _)| *target == key) {
            let value = py_strip(parts.next().unwrap_or_default()).trim_end_matches(';');
            current.retain(|(existing, _)| *existing != key);
            current.push((key, value));
        }
    }
    Ok(targets.iter().all(|(key, wanted)| current.iter().any(|(found, value)| found == key && value == wanted)))
}

/// Rewrites every line with a target key as `"<key> = <value>;\n"`, appends missing keys after a blank line,
/// and writes the file back in the detected encoding; no backup.
/// `Ok(false)` when the game is not installed or reading or writing fails.
pub fn patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let Some(path) = config(env).filter(|path| path.is_file()) else { return Ok(false) };
    let targets = target_settings(env.port_text(PortKey::Tdusc)?);
    let encoding = detect_encoding(&path)?;
    let Ok(content) = text::read_text(&path, encoding) else { return Ok(false) };
    let mut found: Vec<&str> = Vec::new();
    let mut rewritten = String::new();
    for line in text::lines_with_ends(&content) {
        let key = py_strip(line.split('=').next().unwrap_or_default());
        match targets.iter().find(|(target, _)| line.contains('=') && *target == key) {
            Some((target, value)) => {
                rewritten.push_str(&format!("{target} = {value};\n"));
                found.push(target);
            }
            None => rewritten.push_str(line),
        }
    }
    let missing: Vec<&(&str, String)> = targets.iter().filter(|(key, _)| !found.contains(key)).collect();
    if !missing.is_empty() {
        rewritten.push('\n');
        for (key, value) in missing {
            rewritten.push_str(&format!("{key} = {value};\n"));
        }
    }
    Ok(text::write_text(&path, &rewritten, encoding, Newline::Windows).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::testing::{ports, prefix_fixture};

    fn utf16(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    }

    #[test]
    fn native_live_port_text_preserves_tdusc_values_and_file_gate() {
        let prefix = prefix_fixture("tdusc-native-port");
        let dir = prefix.documents().join("My Games/Test Drive Unlimited Solar Crown");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("UserSettings.cfg");
        for raw in ["001337", "+0070000", "-1", "\" 001337 \""] {
            let parsed = crate::import::configobj::parse(&format!("[NETWORK_PORT]\ntdusc = {raw}\n")).unwrap();
            let value = parsed.get("NETWORK_PORT").unwrap().as_dict().unwrap().get("tdusc").unwrap().clone();
            let wanted = value.py_str().unwrap();
            let mut settings = crate::settings::Settings::default();
            settings.edit_native(|config| {
                config.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("tdusc", value);
            });
            let loaded = ModulePorts::at_startup(&settings);
            let env = PatchEnvironment {
                prefix: Some(&prefix),
                steam: None,
                library: &crate::patch::library::BUILTIN,
                ports: &settings,
                loaded_ports: &loaded,
            };
            fs::write(&file, utf16("Rc2.Audio.Volume = 1;\r\n")).unwrap();
            assert!(patch(&env).unwrap());
            let written = text::read_text(&file, Encoding::Utf16).unwrap();
            assert!(written.contains(&format!("Rc2.Telemetry.TelemetryPort = {wanted};\n")), "{raw}: {written}");
            // The file value is trimmed for the comparison; the target is compared as it is.
            assert_eq!(patched(&env).unwrap(), wanted.trim() == wanted);
        }
        fs::remove_file(&file).unwrap();
        let missing = |_: PortKey| None;
        let loaded = ModulePorts::at_startup(&missing);
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &missing,
            loaded_ports: &loaded,
        };
        assert!(!patch(&env).unwrap());
        assert!(!patched(&env).unwrap());
        fs::write(&file, b"bad").unwrap();
        assert!(matches!(patch(&env), Err(PatchError::MissingPort(PortKey::Tdusc))));
    }

    /// Patch: UTF-16 file detected and kept, keys rewritten, missing ones appended after a blank line.
    #[test]
    fn utf16_settings() {
        let prefix = prefix_fixture("tdusc");
        let dir = prefix.documents().join("My Games/Test Drive Unlimited Solar Crown");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("UserSettings.cfg");
        fs::write(&file, utf16("Rc2.Audio.Volume = 1;\r\nRc2.Telemetry.TelemetryPort = 1;\r\n")).unwrap();
        let lookup = ports(20800);
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
        let expected = "Rc2.Audio.Volume = 1;\r\nRc2.Telemetry.TelemetryPort = 20800;\r\n\r\n\
Rc2.Telemetry.TelemetryAdress = \"127.0.0.1\";\r\nRc2.Telemetry.EnableTelemetry = true;\r\nRc2.Telemetry.TelemetryRate = 60;\r\n";
        assert_eq!(fs::read(&file).unwrap(), utf16(expected));
        assert!(patched(&env).unwrap());
    }

    /// Encoding detection: an odd-length UTF-8 file is not UTF-16.
    #[test]
    fn utf8_fallback() {
        let prefix = prefix_fixture("tdusc-utf8");
        let dir = prefix.documents().join("My Games/Test Drive Unlimited Solar Crown");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("UserSettings.cfg"), "a = 1;\n").unwrap();
        assert_eq!(detect_encoding(&dir.join("UserSettings.cfg")).unwrap(), Encoding::Utf8);
    }
}

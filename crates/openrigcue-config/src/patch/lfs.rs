//! Live for Speed `cfg.txt`: OutSim and OutGauge to the configured port.

use std::path::PathBuf;

use super::env::{PatchEnvironment, PortKey, PortModule};
use super::text::{self, Encoding, Newline};
use super::winpath;
use super::PatchError;

/// Registry key whose default value is the install folder.
const INSTALL_KEY: &str = "Software\\Live for Speed";
const CFG_FILE: &str = "cfg.txt";

/// The settings in written order; `None` is the configured port.
const SETTINGS: [(&str, Option<&str>); 10] = [
    ("OutSim Mode", Some("2")),
    ("OutSim Delay", Some("1")),
    ("OutSim IP", Some("127.0.0.1")),
    ("OutSim Port", None),
    ("OutSim ID", Some("0")),
    ("OutGauge Mode", Some("2")),
    ("OutGauge Delay", Some("1")),
    ("OutGauge IP", Some("127.0.0.1")),
    ("OutGauge Port", None),
    ("OutGauge ID", Some("0")),
];

fn install_path(env: &PatchEnvironment<'_>) -> Option<PathBuf> {
    let prefix = env.prefix?;
    prefix.unix_path(prefix.hive().string_value(INSTALL_KEY, "")?)
}

fn cfg_path(env: &PatchEnvironment<'_>) -> Option<PathBuf> {
    Some(winpath::resolve(&install_path(env)?, CFG_FILE))
}

fn lines(port: &str) -> Vec<String> {
    SETTINGS.iter().map(|(key, value)| format!("{key} {}", value.unwrap_or(port))).collect()
}

/// The install folder is registered and holds `cfg.txt`.
pub fn installed(env: &PatchEnvironment<'_>) -> bool {
    cfg_path(env).is_some_and(|path| path.is_file())
}

/// Every `"<key> <value>"` line, with the port the module loaded, is present; an unreadable file is not patched.
pub fn patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let wanted = lines(&env.loaded_port_text(PortModule::Lfs, PortKey::Lfs)?);
    let Some(content) = cfg_path(env).and_then(|path| text::read_text(&path, Encoding::Ansi).ok()) else {
        return Ok(false);
    };
    let present: Vec<&str> =
        content.split_inclusive('\n').map(|line| line.strip_suffix('\n').unwrap_or(line)).collect();
    Ok(wanted.iter().all(|line| present.contains(&line.as_str())))
}

/// Reloads the port, then for each setting in order drops every line containing its key and appends
/// `"<key> <value>"`; no backup.
///
/// The install lookup precedes port reload; joining a missing install path fails only after
/// the cache update. A failed read or write is `Ok(false)`.
pub fn patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let install = install_path(env);
    let port = env.reload_port_text(PortModule::Lfs, PortKey::Lfs)?;
    let path = winpath::resolve(&install.ok_or(PatchError::NotInstalled)?, CFG_FILE);
    let Ok(content) = text::read_text(&path, Encoding::Ansi) else { return Ok(false) };
    let mut kept: Vec<String> =
        content.split_inclusive('\n').map(|line| line.strip_suffix('\n').unwrap_or(line).to_owned()).collect();
    for ((key, _), line) in SETTINGS.iter().zip(lines(&port)) {
        kept.retain(|existing| !existing.contains(key));
        kept.push(line);
    }
    let rewritten: String = kept.iter().map(|line| format!("{line}\n")).collect();
    Ok(text::write_text(&path, &rewritten, Encoding::Ansi, Newline::Windows).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::prefix::WinePrefix;
    use crate::patch::testing::{ports, scratch};
    use std::fs;

    #[test]
    fn native_cached_lfs_keeps_raw_text_until_patch_reloads_both_lines() {
        let root = scratch("lfs-native-cache");
        fs::create_dir_all(root.join("drive_c/LFS")).unwrap();
        fs::write(root.join("user.reg"), "[Software\\\\Live for Speed] 1\n@=\"C:\\\\LFS\"\n").unwrap();
        let file = root.join("drive_c/LFS/cfg.txt");
        fs::write(&file, "Game Res 1\n").unwrap();
        let prefix = WinePrefix::open(root);
        let mut settings = crate::settings::Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("lfs", "001337".into());
        });
        let loaded = ModulePorts::imported(&settings).unwrap();
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &settings,
            loaded_ports: &loaded,
        };
        patch(&env).unwrap();
        assert!(fs::read_to_string(&file).unwrap().contains("OutSim Port 001337\r\n"));
        assert!(fs::read_to_string(&file).unwrap().contains("OutGauge Port 001337\r\n"));
        let mut changed = settings.clone();
        changed.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("lfs", "+001338".into());
        });
        let after = PatchEnvironment { ports: &changed, ..env };
        assert!(patched(&after).unwrap());
        patch(&after).unwrap();
        assert!(fs::read_to_string(&file).unwrap().contains("OutSim Port +001338\r\n"));
        assert!(fs::read_to_string(&file).unwrap().contains("OutGauge Port +001338\r\n"));
        assert!(patched(&after).unwrap());
    }

    #[test]
    fn native_cached_lfs_reloads_before_missing_install_path_join() {
        let prefix = WinePrefix::open(scratch("lfs-native-missing-install"));
        let mut startup = crate::settings::Settings::default();
        startup.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("lfs", "001337".into());
        });
        let loaded = ModulePorts::imported(&startup).unwrap();
        let mut changed = startup;
        changed.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("lfs", "+001338".into());
        });
        let env = PatchEnvironment {
            prefix: Some(&prefix),
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &changed,
            loaded_ports: &loaded,
        };
        assert!(matches!(patch(&env), Err(PatchError::NotInstalled)));
        assert_eq!(loaded.loaded_text(PortModule::Lfs, PortKey::Lfs).unwrap(), "+001338");
        let mut missing = changed.clone();
        missing.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().remove("lfs");
        });
        let env = PatchEnvironment { ports: &missing, ..env };
        assert!(matches!(patch(&env), Err(PatchError::MissingPort(PortKey::Lfs))));
        assert_eq!(loaded.loaded_text(PortModule::Lfs, PortKey::Lfs).unwrap(), "+001338");
    }

    /// Patch: matching lines removed wherever they are, settings appended in order, CRLF.
    #[test]
    fn cfg_rewrite() {
        let root = scratch("lfs");
        fs::create_dir_all(root.join("drive_c/LFS")).unwrap();
        fs::write(root.join("user.reg"), "[Software\\\\Live for Speed] 1\n@=\"C:\\\\LFS\"\n").unwrap();
        fs::write(root.join("drive_c/LFS/cfg.txt"), "Game Res 1\nOutSim Mode 0\nOutGauge Port 1\nX OutSim ID 5\n")
            .unwrap();
        let prefix = WinePrefix::open(root.clone());
        let lookup = ports(30000);
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
            fs::read_to_string(root.join("drive_c/LFS/cfg.txt")).unwrap(),
            "Game Res 1\r\nOutSim Mode 2\r\nOutSim Delay 1\r\nOutSim IP 127.0.0.1\r\nOutSim Port 30000\r\nOutSim ID 0\r\n\
OutGauge Mode 2\r\nOutGauge Delay 1\r\nOutGauge IP 127.0.0.1\r\nOutGauge Port 30000\r\nOutGauge ID 0\r\n"
        );
        assert!(patched(&env).unwrap());

        // patched() compares with the port loaded at application start; only patch() reloads the port.
        let changed = ports(30001);
        let after_change = PatchEnvironment { ports: &changed, ..env };
        assert!(patched(&after_change).unwrap());
        patch(&after_change).unwrap();
        assert!(fs::read_to_string(root.join("drive_c/LFS/cfg.txt")).unwrap().contains("OutSim Port 30001\r\n"));
        assert!(patched(&after_change).unwrap());
    }
}

//! IL-2 Sturmovik (Steam) `data\startup.cfg`: the motion and telemetry device blocks.

use std::path::PathBuf;

use super::env::{PatchEnvironment, PortKey, PortModule};
use super::text::{self, Encoding, Newline};
use super::winpath;
use super::PatchError;

/// Steam location used to find the game.
const GAME_LOCATION: &str = "IL-2 Sturmovik Battle of Stalingrad\\data\\startup.cfg";
const STARTUP_CFG: &str = "data\\startup.cfg";
const END_MARKER: &str = "[END]";

/// The two device sections, in the order they are written.
const DEVICES: [&str; 2] = ["motiondevice", "telemetrydevice"];

/// A device block as written to the file (tab-indented, `\n` line ends).
fn device_block(device: &str, port: &str) -> String {
    format!(
        "[KEY = {device}]\n\taddr = \"127.0.0.1\"\n\tdecimation = 1\n\tenable = true\n\tport = {port}\n{END_MARKER}"
    )
}

fn startup_cfg(env: &PatchEnvironment<'_>) -> Option<PathBuf> {
    let game = env.steam?.find_game(GAME_LOCATION)?;
    Some(winpath::resolve(&game, STARTUP_CFG))
}

/// The game is in a Steam library.
pub fn installed(env: &PatchEnvironment<'_>) -> bool {
    env.steam.is_some_and(|steam| steam.find_game(GAME_LOCATION).is_some())
}

/// Both blocks, from their header to the first `[END]` after it, equal the blocks built with the port the module
/// loaded; any failure is false.
pub fn patched(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let port = env.loaded_port_text(PortModule::Il2, PortKey::Il2)?;
    let Some(content) = startup_cfg(env).and_then(|path| text::read_text(&path, Encoding::Ansi).ok()) else {
        return Ok(false);
    };
    Ok(DEVICES.iter().all(|device| extract_block(&content, device).as_deref() == Some(&device_block(device, &port))))
}

/// Reloads the port, removes both blocks and appends `\n` + motion block + `\n\n` + telemetry block; no backup.
///
/// `Ok(false)` when the game is not found through Steam, or on any failure
/// reading or writing the file.
pub fn patch(env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
    let port = env.reload_port_text(PortModule::Il2, PortKey::Il2)?;
    let Some(path) = startup_cfg(env) else { return Ok(false) };
    let Ok(content) = text::read_text(&path, Encoding::Ansi) else { return Ok(false) };
    let stripped = DEVICES.iter().fold(content, |text, device| remove_block(&text, device));
    let rewritten = format!("{stripped}\n{}\n\n{}", device_block(DEVICES[0], &port), device_block(DEVICES[1], &port));
    Ok(text::write_text(&path, &rewritten, Encoding::Ansi, Newline::Windows).is_ok())
}

fn header(device: &str) -> String {
    format!("[KEY = {device}]")
}

/// Text from the block header to the end of the first `[END]` after it.
fn extract_block(content: &str, device: &str) -> Option<String> {
    let start = content.find(&header(device))?;
    let end = content[start..].find(END_MARKER)?;
    Some(content[start..start + end + END_MARKER.len()].to_owned())
}

/// Removes the first block and the one character after its `[END]`; without an `[END]` the computed end is four
/// characters past the header start.
fn remove_block(content: &str, device: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let header: Vec<char> = header(device).chars().collect();
    let Some(start) = find_chars(&chars, &header, 0) else {
        return content.to_owned();
    };
    let marker: Vec<char> = END_MARKER.chars().collect();
    let end = match find_chars(&chars, &marker, start) {
        Some(found) => found + marker.len(),
        None => start + marker.len() - 1,
    };
    let resume = (end + 1).min(chars.len());
    chars[..start].iter().chain(&chars[resume..]).collect()
}

fn find_chars(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    (from..=haystack.len().checked_sub(needle.len())?).find(|&index| haystack[index..index + needle.len()] == *needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::steam::SteamLibraries;
    use crate::patch::testing::{ports, scratch};
    use std::fs;

    /// Block removal: block plus the following character removed; the missing-[END] case.
    #[test]
    fn remove_sections() {
        let text = "a\n[KEY = motiondevice]\n\tx\n[END]\nb";
        assert_eq!(remove_block(text, "motiondevice"), "a\nb");
        assert_eq!(remove_block("x[KEY = motiondevice]abcdef", "motiondevice"), "x= motiondevice]abcdef");
        assert_eq!(remove_block("none", "motiondevice"), "none");
    }

    #[test]
    fn native_cached_il2_keeps_raw_block_text_until_patch_reload() {
        let base = scratch("il2-native-cache");
        let data = base.join("Steam/steamapps/common/IL-2 Sturmovik Battle of Stalingrad/data");
        fs::create_dir_all(&data).unwrap();
        let file = data.join("startup.cfg");
        fs::write(&file, "[KEY = graphics]\n[END]\n").unwrap();
        let steam = SteamLibraries::of(&base.join("Steam"));
        let mut settings = crate::settings::Settings::default();
        settings.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("il2", "001337".into());
        });
        let loaded = ModulePorts::imported(&settings).unwrap();
        let env = PatchEnvironment {
            prefix: None,
            steam: Some(&steam),
            library: &crate::patch::library::BUILTIN,
            ports: &settings,
            loaded_ports: &loaded,
        };
        patch(&env).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap().matches("port = 001337\r\n").count(), 2);
        let mut changed = settings.clone();
        changed.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("il2", "+001338".into());
        });
        let after = PatchEnvironment { ports: &changed, ..env };
        assert!(patched(&after).unwrap());
        patch(&after).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap().matches("port = +001338\r\n").count(), 2);
        assert!(patched(&after).unwrap());
    }

    #[test]
    fn native_cached_il2_reloads_before_a_missing_steam_target() {
        let mut startup = crate::settings::Settings::default();
        startup.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("il2", "001337".into());
        });
        let loaded = ModulePorts::imported(&startup).unwrap();
        let mut changed = startup;
        changed.edit_native(|raw| {
            raw.get_mut("NETWORK_PORT").unwrap().as_dict_mut().unwrap().insert("il2", "+001338".into());
        });
        let env = PatchEnvironment {
            prefix: None,
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &changed,
            loaded_ports: &loaded,
        };
        assert!(!patch(&env).unwrap());
        assert_eq!(loaded.loaded_text(PortModule::Il2, PortKey::Il2).unwrap(), "+001338");
    }

    /// Patch / patched against a Steam library layout.
    #[test]
    fn startup_cfg_blocks() {
        let base = scratch("il2");
        let data = base.join("Steam/steamapps/common/IL-2 Sturmovik Battle of Stalingrad/data");
        fs::create_dir_all(&data).unwrap();
        fs::write(
            data.join("startup.cfg"),
            "[KEY = graphics]\r\n\tfps = 60\r\n[END]\r\n[KEY = motiondevice]\r\n\taddr = \"1.2.3.4\"\r\n[END]\r\n",
        )
        .unwrap();
        let steam = SteamLibraries::of(&base.join("Steam"));
        let lookup = ports(29373);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = PatchEnvironment {
            prefix: None,
            steam: Some(&steam),
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        assert!(installed(&env));
        assert!(!patched(&env).unwrap());
        patch(&env).unwrap();
        let written = fs::read_to_string(data.join("startup.cfg")).unwrap();
        assert_eq!(
            written,
            "[KEY = graphics]\r\n\tfps = 60\r\n[END]\r\n\r\n[KEY = motiondevice]\r\n\taddr = \"127.0.0.1\"\r\n\t\
decimation = 1\r\n\tenable = true\r\n\tport = 29373\r\n[END]\r\n\r\n[KEY = telemetrydevice]\r\n\taddr = \
\"127.0.0.1\"\r\n\tdecimation = 1\r\n\tenable = true\r\n\tport = 29373\r\n[END]"
        );
        assert!(patched(&env).unwrap());

        // patched() compares with the blocks built from the loaded port; patch() rebuilds them.
        let changed = ports(29374);
        let after_change = PatchEnvironment { ports: &changed, ..env };
        assert!(patched(&after_change).unwrap());
        patch(&after_change).unwrap();
        assert!(fs::read_to_string(data.join("startup.cfg")).unwrap().contains("port = 29374\r\n"));
        assert!(patched(&after_change).unwrap());
    }
}

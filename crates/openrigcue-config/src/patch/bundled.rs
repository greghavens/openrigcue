//! Plugin files copied from the asset library into a Documents folder: Aerofly FS 2 and Farming Simulator
//! 19/22/25.

use std::fs;
use std::io;
use std::path::PathBuf;

use super::env::{PatchEnvironment, UserFolder, UserPath};
use super::files::backup_path;
use super::winpath;
use super::PatchError;

/// One copied plugin file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledFile {
    /// Game folder below Documents.
    pub game_dir: &'static str,
    /// Sub-folder receiving the file.
    pub plugin_dir: &'static str,
    /// File name, identical in the asset library.
    pub file: &'static str,
    /// The game's name in the module's messages.
    pub name: &'static str,
    /// The module's message for a failed copy.
    pub copy_error: &'static str,
}

impl BundledFile {
    fn game_dir(&self, env: &PatchEnvironment<'_>) -> Option<PathBuf> {
        env.prefix.map(|prefix| UserPath::new(UserFolder::Documents, self.game_dir).resolve(prefix))
    }

    fn plugin_dir(&self, env: &PatchEnvironment<'_>) -> Option<PathBuf> {
        Some(winpath::resolve(&self.game_dir(env)?, self.plugin_dir))
    }

    /// The game folder exists.
    pub fn installed(&self, env: &PatchEnvironment<'_>) -> bool {
        self.game_dir(env).is_some_and(|dir| dir.is_dir())
    }

    /// The plugin file exists and equals the library file.
    pub fn patched(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        let Some(dir) = self.plugin_dir(env).filter(|dir| dir.is_dir()) else {
            return Ok(false);
        };
        let installed = winpath::resolve(&dir, self.file);
        if !installed.is_file() {
            return Ok(false);
        }
        Ok(env.library.same_as(&installed, self.file)?)
    }

    /// Creates the plugin folder, moves an existing file to its backup (none is fine), then copies the library
    /// file in; the module's messages are printed. Each failure is `Ok(false)`.
    pub fn patch(&self, env: &PatchEnvironment<'_>) -> Result<bool, PatchError> {
        let dir = self.plugin_dir(env).ok_or(PatchError::NotInstalled)?;
        if !dir.is_dir() && fs::create_dir(&dir).is_err() {
            return Ok(false);
        }
        let target = winpath::resolve(&dir, self.file);
        match fs::rename(&target, backup_path(&target, self.file)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::info!("{} There is nothing to do backup {error}", self.name);
            }
            Err(error) => {
                tracing::info!("{} There is an error copying the file {error}", self.name);
                return Ok(false);
            }
        }
        // The copy gets the module's own file name, whatever the case of the file just moved aside.
        match env.library.copy_to(self.file, &winpath::resolve(&dir, self.file)) {
            Ok(()) => Ok(true),
            Err(error) => {
                tracing::info!("{} {error}", self.copy_error);
                Ok(false)
            }
        }
    }
}

pub const AEROFLY_FS2: BundledFile = BundledFile {
    game_dir: "Aerofly FS 2",
    plugin_dir: "external_dll",
    file: "Aerofly_FS_2_GamePlugin_Telemetry.dll",
    name: "Aerofly FS2",
    copy_error: "DCS Patch Error while copying the file",
};
/// fs19 prints the Farming Simulator 22 texts.
pub const FARMING_SIMULATOR_19: BundledFile = farming_simulator(
    "My Games\\FarmingSimulator2019",
    "FS19_TelemetryMod.zip",
    "Farming Simulator 22",
    "Farming Simulator 22 Patch Error while copying the file",
);
pub const FARMING_SIMULATOR_22: BundledFile = farming_simulator(
    "My Games\\FarmingSimulator2022",
    "FS22_TelemetryMod.zip",
    "Farming Simulator 22",
    "Farming Simulator 22 Patch Error while copying the file",
);
pub const FARMING_SIMULATOR_25: BundledFile = farming_simulator(
    "My Games\\FarmingSimulator2025",
    "FS25_TelemetryMod.zip",
    "Farming Simulator 25",
    "Farming Simulator 25 Patch Error while copying the file",
);

const fn farming_simulator(
    game_dir: &'static str,
    file: &'static str,
    name: &'static str,
    copy_error: &'static str,
) -> BundledFile {
    BundledFile { game_dir, plugin_dir: "mods", file, name, copy_error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::env::ModulePorts;
    use crate::patch::library::{self, Library, BUILTIN};
    use crate::patch::testing::{ports, prefix_fixture, scratch};

    fn environment<'a>(
        prefix: &'a crate::patch::prefix::WinePrefix,
        library: &'a Library,
        lookup: &'a dyn crate::patch::env::PortLookup,
        loaded: &'a ModulePorts,
    ) -> PatchEnvironment<'a> {
        PatchEnvironment { prefix: Some(prefix), steam: None, library, ports: lookup, loaded_ports: loaded }
    }

    /// fs22 patch / patched with the built-in mod: mods folder created, old zip backed up, library zip compared.
    #[test]
    fn farming_simulator_mod() {
        let prefix = prefix_fixture("bundled-fs");
        let lookup = ports(0);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = environment(&prefix, &BUILTIN, &lookup, &loaded);
        assert!(!FARMING_SIMULATOR_22.installed(&env));
        let game = prefix.documents().join("My Games/FarmingSimulator2022");
        fs::create_dir_all(&game).unwrap();
        assert!(FARMING_SIMULATOR_22.installed(&env));
        assert!(!FARMING_SIMULATOR_22.patched(&env).unwrap());
        assert!(FARMING_SIMULATOR_22.patch(&env).unwrap());
        assert!(FARMING_SIMULATOR_22.patched(&env).unwrap());
        fs::write(game.join("mods/FS22_TelemetryMod.zip"), b"PK old").unwrap();
        assert!(!FARMING_SIMULATOR_22.patched(&env).unwrap());
        FARMING_SIMULATOR_22.patch(&env).unwrap();
        assert_eq!(fs::read(game.join("mods/FS22_TelemetryModsrs_bck.zip")).unwrap(), b"PK old");
        assert_eq!(
            fs::read(game.join("mods/FS22_TelemetryMod.zip")).unwrap(),
            library::file("FS22_TelemetryMod.zip").unwrap()
        );
    }

    /// Aerofly FS 2 patch: the game's DLL is moved to its backup before the library file is copied, so a library
    /// without the file leaves the DLL moved aside and fails.
    #[test]
    fn copy_failure_after_the_move() {
        let prefix = prefix_fixture("bundled-aerofly");
        let library = Library::Folder(scratch("bundled-aerofly-lib"));
        let lookup = ports(0);
        let loaded = ModulePorts::at_startup(&lookup);
        let env = environment(&prefix, &library, &lookup, &loaded);
        let dlls = prefix.documents().join("Aerofly FS 2/external_dll");
        fs::create_dir_all(&dlls).unwrap();
        fs::write(dlls.join("Aerofly_FS_2_GamePlugin_Telemetry.dll"), b"MZ old").unwrap();
        assert!(!AEROFLY_FS2.patch(&env).unwrap());
        assert!(!dlls.join("Aerofly_FS_2_GamePlugin_Telemetry.dll").exists());
        assert_eq!(fs::read(dlls.join("Aerofly_FS_2_GamePlugin_Telemetrysrs_bck.dll")).unwrap(), b"MZ old");
    }
}

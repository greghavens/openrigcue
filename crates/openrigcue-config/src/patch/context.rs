//! Which Wine/Proton prefix a game's files live in.
//!
//! On Windows every game shares one user account. On Linux each Proton game has its
//! own prefix (`steamapps/compatdata/<appid>/pfx`), and non-Steam games use whatever prefix their launcher created,
//! so a module is pointed at the first candidate prefix in which the game is installed.

use super::env::{ModulePorts, PatchEnvironment, PortLookup};
use super::games::Patcher;
use super::prefix::WinePrefix;
use super::steam::SteamLibraries;

/// Everything the patch modules read, with every candidate prefix instead of one.
pub struct PatchContext<'a> {
    /// Candidate prefixes in search order, normally [`super::candidate_prefixes`]: the Proton prefixes of
    /// every Steam library, then the Wine prefixes of other launchers.
    pub prefixes: &'a [WinePrefix],
    pub steam: Option<&'a SteamLibraries>,
    /// The templates and plugin files.
    pub library: &'a super::library::Library,
    pub ports: &'a dyn PortLookup,
    pub loaded_ports: &'a ModulePorts,
}

impl<'a> PatchContext<'a> {
    fn environment(&self, prefix: Option<&'a WinePrefix>) -> PatchEnvironment<'a> {
        PatchEnvironment {
            prefix,
            steam: self.steam,
            library: self.library,
            ports: self.ports,
            loaded_ports: self.loaded_ports,
        }
    }

    /// The environment a module runs in: the first prefix in which [`Patcher::installed`] holds, or no prefix
    /// when the game is installed in none (its `installed()` and `patched()` are then false, its `patch()` fails).
    /// A module that finds its game through Steam ([`Patcher::locates_through_steam`]) always runs in the first
    /// prefix, whose `C:` drive is the fallback drive root.
    pub fn environment_for(&self, patcher: &Patcher) -> PatchEnvironment<'a> {
        if patcher.locates_through_steam() {
            return self.environment(self.prefixes.first());
        }
        let prefix = self.prefixes.iter().find(|prefix| patcher.installed(&self.environment(Some(prefix))));
        self.environment(prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::games::{EntryKind, GAME_ENTRIES};
    use crate::patch::testing::{ports, scratch};
    use crate::patch::{codemasters, template};
    use std::fs;

    /// Proton layout with two prefixes: DiRT Rally 2.0 (app 690790) in the second, The Crew 2 (app 646910) in the
    /// first; each module finds its own.
    #[test]
    fn first_prefix_where_the_game_is_installed() {
        let steam = scratch("ctx-two-prefixes");
        let compatdata = steam.join("steamapps/compatdata");
        let documents = |app: &str| compatdata.join(app).join("pfx/drive_c/users/steamuser/Documents");
        fs::create_dir_all(documents("646910").join("The Crew 2")).unwrap();
        let dirt = documents("690790").join("My Games/DiRT Rally 2.0/hardwaresettings");
        fs::create_dir_all(&dirt).unwrap();
        fs::write(dirt.join("hardware_settings_config.xml"), "<hardware_settings_config/>").unwrap();

        let libraries = SteamLibraries::of(&steam);
        let prefixes = libraries.proton_prefixes();
        assert_eq!(prefixes.len(), 2);
        let lookup = ports(20777);
        let loaded = ModulePorts::at_startup(&lookup);
        let context = PatchContext {
            prefixes: &prefixes,
            steam: Some(&libraries),
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };

        let dirt_env = context.environment_for(&Patcher::Codemasters(&codemasters::DIRT_RALLY2_STEAM));
        assert_eq!(dirt_env.prefix.map(WinePrefix::root), Some(compatdata.join("690790/pfx").as_path()));
        let crew_env = context.environment_for(&Patcher::Template(&template::THE_CREW_2));
        assert_eq!(crew_env.prefix.map(WinePrefix::root), Some(compatdata.join("646910/pfx").as_path()));
        let absent = context.environment_for(&Patcher::Codemasters(&codemasters::F12025));
        assert!(absent.prefix.is_none());
    }

    /// Game status over a machine with no game: every module's patched() answers without an error
    /// for a game that is not installed.
    #[test]
    fn nothing_installed_is_not_an_error() {
        let lookup = ports(1);
        let loaded = ModulePorts::at_startup(&lookup);
        let context = PatchContext {
            prefixes: &[],
            steam: None,
            library: &crate::patch::library::BUILTIN,
            ports: &lookup,
            loaded_ports: &loaded,
        };
        for entry in GAME_ENTRIES {
            if let EntryKind::Config(patcher) | EntryKind::Dll(patcher) = entry.kind {
                let env = context.environment_for(&patcher);
                assert!(!patcher.installed(&env), "{}", entry.key);
                assert!(!patcher.patched(&env).unwrap(), "{}", entry.key);
            }
        }
    }
}

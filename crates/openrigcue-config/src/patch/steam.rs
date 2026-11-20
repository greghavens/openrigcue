//! Steam game discovery and the plugin-copy patches of Steam games.

use std::fs;
use std::path::{Path, PathBuf};

use super::library::Library;
use super::prefix::WinePrefix;
use super::text::{self, Encoding};
use super::vdf::{self, Node};
use super::winpath;
use super::PatchError;

/// Steam installations of a Linux desktop, in the order they are looked for.
const STEAM_ROOTS: [&str; 3] =
    [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"];

/// The Steam installation and its library folders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SteamLibraries {
    /// Search roots in order: `<steam>/steamapps/common/`, then every library `path` of `libraryfolders.vdf`.
    pub search_roots: Vec<PathBuf>,
    /// Library folders (`<steam>` and every `path` of `libraryfolders.vdf`), for Proton prefixes.
    pub library_folders: Vec<PathBuf>,
}

impl SteamLibraries {
    /// Finds the Steam installation under `home` (the Linux counterpart of the `SteamPath` registry value).
    pub fn discover(home: &Path) -> Option<Self> {
        STEAM_ROOTS.iter().map(|relative| home.join(relative)).find(|root| root.is_dir()).map(|root| Self::of(&root))
    }

    /// Library folders of a Steam installation.
    ///
    /// The main `steamapps/common/` when it exists, then the `path` of every
    /// numeric key of the `libraryfolders` section in order; the file is read in the ANSI code page
    /// (so UTF-8 paths with non-ASCII characters come out as mojibake), a decode or parse
    /// error keeps only the main folder and a numeric entry without `path` ends the list.
    pub fn of(steam_root: &Path) -> Self {
        let mut search_roots = Vec::new();
        let main = steam_root.join("steamapps").join("common");
        if main.is_dir() {
            search_roots.push(main);
        }
        let mut library_folders = vec![steam_root.to_path_buf()];
        let listed = text::read_text(&steam_root.join("steamapps").join("libraryfolders.vdf"), Encoding::Ansi)
            .ok()
            .and_then(|text| vdf::parse(&text))
            .map(|root| listed_libraries(&root))
            .unwrap_or_default();
        for folder in listed {
            if !library_folders.contains(&folder) {
                library_folders.push(folder.clone());
            }
            search_roots.push(folder);
        }
        SteamLibraries { search_roots, library_folders }
    }

    /// Directory two levels above `plugin_location` (a Windows relative path such as
    /// `rFactor\Plugins\SRS_Rfactor1.dll`) in the first library that has it, tried directly under the library and
    /// under its `steamapps/common`.
    pub fn find_game(&self, plugin_location: &str) -> Option<PathBuf> {
        let game_dir = winpath::without_last(plugin_location, 2);
        self.search_roots.iter().find_map(|root| {
            [winpath::resolve(root, &game_dir), winpath::resolve(&root.join("steamapps").join("common"), &game_dir)]
                .into_iter()
                .find(|candidate| candidate.is_dir())
        })
    }

    /// Proton prefixes of every library (`steamapps/compatdata/<appid>/pfx`), by library then app id.
    pub fn proton_prefixes(&self) -> Vec<WinePrefix> {
        self.library_folders
            .iter()
            .flat_map(|folder| prefixes_in(&folder.join("steamapps").join("compatdata")))
            .collect()
    }
}

fn listed_libraries(root: &Node) -> Vec<PathBuf> {
    let Some(Node::Section(entries)) = root.get("libraryfolders") else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    for (key, entry) in entries {
        if key.is_empty() || !key.chars().all(char::is_numeric) {
            continue;
        }
        let Some(path) = entry.get("path").and_then(Node::text) else {
            break;
        };
        folders.push(PathBuf::from(path));
    }
    folders
}

fn prefixes_in(compatdata: &Path) -> Vec<WinePrefix> {
    let Ok(entries) = fs::read_dir(compatdata) else {
        return Vec::new();
    };
    let mut apps: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.file_name().to_str()?.parse().ok()?, entry.path().join("pfx"))))
        .filter(|(_, pfx)| pfx.is_dir())
        .collect();
    apps.sort();
    apps.into_iter().map(|(_, pfx)| WinePrefix::open(pfx)).collect()
}

/// A plugin file copied from the asset library into a Steam game's folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamPlugin {
    /// Plugin path relative to a library.
    pub location: &'static str,
}

impl SteamPlugin {
    fn file_name(self) -> &'static str {
        winpath::file_name(self.location)
    }

    /// The game is installed in a Steam library.
    pub fn installed(self, steam: &SteamLibraries) -> bool {
        steam.find_game(self.location).is_some()
    }

    /// Name of the plugin's own folder (the last folder of `plugin_location`).
    fn plugin_folder(self) -> &'static str {
        let without_file = &self.location[..self.location.len() - self.file_name().len()];
        winpath::file_name(without_file.trim_end_matches(['\\', '/']))
    }

    /// Plugin folder of the installed game: the game path joined with the plugin's own folder name.
    pub fn plugin_dir(self, steam: &SteamLibraries) -> Option<PathBuf> {
        let game = steam.find_game(self.location)?;
        Some(winpath::resolve(&game, self.plugin_folder()))
    }

    /// The plugin folder exists and holds a copy of the library file; any failure is false.
    pub fn patched(self, steam: &SteamLibraries, library: &Library) -> bool {
        let Some(dir) = self.plugin_dir(steam).filter(|dir| dir.is_dir()) else {
            return false;
        };
        let installed = winpath::resolve(&dir, self.file_name());
        library.same_as(&installed, self.file_name()).unwrap_or(false)
    }

    /// Creates the plugin folder when missing and copies the library file into it, without a backup.
    ///
    /// When Steam has libraries but none holds the game, the game path is empty, so
    /// `\<plugin folder>` is created at the drive root and the file copied there;
    /// `drive_root` is that root (the `C:` drive of the prefix the module runs in). Without any library the
    /// patch is an error.
    pub fn patch(
        self,
        steam: &SteamLibraries,
        library: &Library,
        drive_root: Option<&Path>,
    ) -> Result<bool, PatchError> {
        let dir = match self.plugin_dir(steam) {
            Some(dir) => dir,
            None if !steam.search_roots.is_empty() => {
                winpath::resolve(drive_root.ok_or(PatchError::NotInstalled)?, self.plugin_folder())
            }
            None => return Err(PatchError::NotInstalled),
        };
        if !dir.is_dir() {
            fs::create_dir(&dir)?;
        }
        // Only a failed copy is `Ok(false)`.
        Ok(library.copy_to(self.file_name(), &winpath::resolve(&dir, self.file_name())).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn steam_fixture(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("openrigcue-config-b-steam-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let steam = base.join("Steam");
        let second = base.join("SteamLibrary");
        fs::create_dir_all(steam.join("steamapps/common")).unwrap();
        fs::create_dir_all(second.join("steamapps/common/rFactor 2/Bin64/Plugins")).unwrap();
        let vdf = format!(
            "\"libraryfolders\"\n{{\n\"0\" {{ \"path\" \"{}\" }}\n\"1\" {{ \"path\" \"{}\" }}\n}}\n",
            steam.display(),
            second.display()
        );
        fs::write(steam.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
        (base, steam)
    }

    /// Library folders: main common folder first, then every numeric library path.
    #[test]
    fn library_order() {
        let (base, steam) = steam_fixture("order");
        let libraries = SteamLibraries::of(&steam);
        assert_eq!(libraries.search_roots, [steam.join("steamapps/common"), steam.clone(), base.join("SteamLibrary")]);
        assert_eq!(libraries.library_folders, [steam, base.join("SteamLibrary")]);
    }

    /// Library folders: a numeric entry without "path" ends the list; a broken file keeps the main folder.
    #[test]
    fn library_list_edge_cases() {
        let (_, steam) = steam_fixture("edges");
        let vdf = "\"libraryfolders\" { \"contentstatsid\" \"1\" \"0\" { \"label\" \"\" } \"1\" { \"path\" \"/x\" } }";
        fs::write(steam.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
        assert_eq!(SteamLibraries::of(&steam).search_roots, [steam.join("steamapps/common")]);
        fs::write(steam.join("steamapps/libraryfolders.vdf"), "\"libraryfolders\" {").unwrap();
        assert_eq!(SteamLibraries::of(&steam).search_roots, [steam.join("steamapps/common")]);
    }

    /// Library folders: the file is decoded as cp1252; a UTF-8 "é" becomes "Ã©", and a byte cp1252
    /// leaves undefined (0x81) makes the whole file unreadable.
    #[test]
    fn library_file_is_read_as_ansi() {
        let (_, steam) = steam_fixture("ansi");
        fs::write(
            steam.join("steamapps/libraryfolders.vdf"),
            "\"libraryfolders\" { \"0\" { \"path\" \"/games/caf\u{e9}\" } }".as_bytes(),
        )
        .unwrap();
        assert_eq!(
            SteamLibraries::of(&steam).search_roots,
            [steam.join("steamapps/common"), PathBuf::from("/games/caf\u{c3}\u{a9}")]
        );
        let mut undefined = b"\"libraryfolders\" { \"0\" { \"path\" \"/x".to_vec();
        undefined.extend_from_slice(b"\x81\" } }");
        fs::write(steam.join("steamapps/libraryfolders.vdf"), undefined).unwrap();
        assert_eq!(SteamLibraries::of(&steam).search_roots, [steam.join("steamapps/common")]);
    }

    /// Game lookup, patch and patched check: plugin folder two levels down, copied file compared.
    #[test]
    fn plugin_copy() {
        let (base, steam) = steam_fixture("plugin");
        let dir = base.join("libraries");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("rFactor2SharedMemoryMapPlugin64.dll"), b"MZ plugin").unwrap();
        let library = Library::Folder(dir);
        let libraries = SteamLibraries::of(&steam);
        let plugin = SteamPlugin { location: "rFactor 2\\Bin64\\Plugins\\rFactor2SharedMemoryMapPlugin64.dll" };
        let game = base.join("SteamLibrary/steamapps/common/rFactor 2/Bin64");
        assert_eq!(libraries.find_game(plugin.location), Some(game.clone()));
        assert!(plugin.installed(&libraries));
        assert!(!plugin.patched(&libraries, &library));
        plugin.patch(&libraries, &library, None).unwrap();
        assert!(plugin.patched(&libraries, &library));
        assert_eq!(fs::read(game.join("Plugins/rFactor2SharedMemoryMapPlugin64.dll")).unwrap(), b"MZ plugin");

        let missing = SteamPlugin { location: "rFactor\\Plugins\\SRS_Rfactor1.dll" };
        assert!(!missing.installed(&libraries));
        assert!(matches!(missing.patch(&libraries, &library, None), Err(PatchError::NotInstalled)));
        assert!(matches!(missing.patch(&SteamLibraries::default(), &library, None), Err(PatchError::NotInstalled)));
    }

    /// Warplanes WW1 patch with the built-in library: `smc.cfg` copied as it is into `StreamingAssets`.
    #[test]
    fn warplanes_config_from_the_builtin_library() {
        let (base, steam) = steam_fixture("warplanes");
        let library = crate::patch::library::BUILTIN;
        let assets = base.join("SteamLibrary/steamapps/common/Warplanes WW1 Fighters/WW1_Data/StreamingAssets");
        fs::create_dir_all(&assets).unwrap();
        let libraries = SteamLibraries::of(&steam);
        let plugin = crate::patch::games::WARPLANES_WW1;
        assert!(plugin.installed(&libraries));
        assert!(!plugin.patched(&libraries, &library));
        plugin.patch(&libraries, &library, None).unwrap();
        assert!(plugin.patched(&libraries, &library));
        assert_eq!(fs::read_to_string(assets.join("smc.cfg")).unwrap(), "ip=127.0.0.1\nport=50000\nmmf=ww1\n");
        // A binary plugin: the plugin folder is created and the built-in DLL copied.
        let rfactor = base.join("SteamLibrary/steamapps/common/rFactor");
        fs::create_dir_all(&rfactor).unwrap();
        let rfactor_plugin = crate::patch::games::RFACTOR;
        assert!(!rfactor_plugin.patched(&libraries, &library));
        rfactor_plugin.patch(&libraries, &library, None).unwrap();
        assert!(rfactor_plugin.patched(&libraries, &library));
        assert_eq!(
            fs::read(rfactor.join("Plugins/SRS_Rfactor1.dll")).unwrap(),
            crate::patch::library::file("SRS_Rfactor1.dll").unwrap()
        );
    }

    /// Patch for a game no library holds: `\\StreamingAssets` created at the drive root and the file copied
    /// there (the patch reports success); the game itself stays not installed.
    #[test]
    fn game_not_found_copies_to_the_drive_root() {
        let (base, steam) = steam_fixture("not-found");
        let library = crate::patch::library::BUILTIN;
        let drive = base.join("pfx/drive_c");
        fs::create_dir_all(&drive).unwrap();
        let libraries = SteamLibraries::of(&steam);
        let plugin = crate::patch::games::WARPLANES_BOP;
        assert!(!plugin.installed(&libraries));
        plugin.patch(&libraries, &library, Some(&drive)).unwrap();
        assert_eq!(
            fs::read_to_string(drive.join("StreamingAssets/smc.cfg")).unwrap(),
            "ip=127.0.0.1\nport=50000\nmmf=ww1\n"
        );
        assert!(!plugin.installed(&libraries));
        assert_eq!(crate::patch::games::RFACTOR.plugin_folder(), "Plugins");
        assert_eq!(crate::patch::games::ETS2.plugin_folder(), "plugins");
    }

    /// Proton prefixes are listed per library by numeric app id.
    #[test]
    fn proton_prefixes_sorted() {
        let (_, steam) = steam_fixture("prefixes");
        for app in ["2000", "310", "x"] {
            fs::create_dir_all(steam.join("steamapps/compatdata").join(app).join("pfx")).unwrap();
        }
        let roots: Vec<PathBuf> =
            SteamLibraries::of(&steam).proton_prefixes().iter().map(|p| p.root().to_path_buf()).collect();
        assert_eq!(roots, [steam.join("steamapps/compatdata/310/pfx"), steam.join("steamapps/compatdata/2000/pfx")]);
    }
}

//! A Wine/Proton prefix: the Windows user folders, drives and registry the patch modules use.

use std::path::{Path, PathBuf};

use super::registry::UserHive;
use super::winpath;

/// Registry key holding the user's special folders.
const SHELL_FOLDERS: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders";
/// User folder name Proton creates.
const PROTON_USER: &str = "steamuser";

/// A Wine prefix directory (the one holding `drive_c`, `dosdevices` and `user.reg`; for Proton
/// `steamapps/compatdata/<appid>/pfx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinePrefix {
    root: PathBuf,
    hive: UserHive,
}

impl WinePrefix {
    /// Opens a prefix; a missing or unreadable `user.reg` leaves the registry empty.
    pub fn open(root: PathBuf) -> Self {
        let hive = UserHive::load(&root.join("user.reg")).unwrap_or_default();
        WinePrefix { root, hive }
    }

    /// The prefix directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `HKEY_CURRENT_USER` hive.
    pub fn hive(&self) -> &UserHive {
        &self.hive
    }

    /// The user's Documents folder (`CSIDL_PERSONAL`).
    pub fn documents(&self) -> PathBuf {
        self.shell_folder("Personal").unwrap_or_else(|| self.default_profile().join("Documents"))
    }

    /// `%APPDATA%`.
    pub fn app_data(&self) -> PathBuf {
        self.shell_folder("AppData").unwrap_or_else(|| self.default_profile().join("AppData").join("Roaming"))
    }

    /// `%LOCALAPPDATA%`.
    pub fn local_app_data(&self) -> PathBuf {
        self.shell_folder("Local AppData").unwrap_or_else(|| self.default_profile().join("AppData").join("Local"))
    }

    /// The user profile folder: the folder that holds Documents.
    pub fn profile(&self) -> PathBuf {
        self.documents().parent().map(Path::to_path_buf).unwrap_or_else(|| self.default_profile())
    }

    /// Maps a Windows absolute path (`C:\…`, `z:/…`) to the Linux path behind the prefix's drive letters.
    pub fn unix_path(&self, windows_path: &str) -> Option<PathBuf> {
        let mut chars = windows_path.chars();
        let (letter, colon) = (chars.next()?, chars.next()?);
        if !letter.is_ascii_alphabetic() || colon != ':' {
            return None;
        }
        let drive = self.root.join("dosdevices").join(format!("{}:", letter.to_ascii_lowercase()));
        let drive =
            if drive.exists() || !letter.eq_ignore_ascii_case(&'c') { drive } else { self.root.join("drive_c") };
        Some(winpath::resolve(&drive, &windows_path[2..]))
    }

    fn shell_folder(&self, name: &str) -> Option<PathBuf> {
        self.unix_path(self.hive.string_value(SHELL_FOLDERS, name)?)
    }

    /// `drive_c/users/steamuser` (Proton), else the first other user folder Wine created (named after the
    /// Linux user), used when the registry names no folder.
    fn default_profile(&self) -> PathBuf {
        let users = self.root.join("drive_c").join("users");
        let proton = users.join(PROTON_USER);
        if proton.is_dir() {
            return proton;
        }
        let mut others: Vec<PathBuf> = std::fs::read_dir(&users)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.file_name() != "Public" && entry.path().is_dir())
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default();
        others.sort();
        others.into_iter().next().unwrap_or(proton)
    }
}

/// Wine prefixes of launchers other than Steam, in this order: `$WINEPREFIX`, `~/.wine`, the `prefix` of every
/// Lutris game configuration, the `winePrefix` of every Heroic game configuration and Heroic's default prefix
/// folder, every Bottles bottle (native and Flatpak installs). Only folders holding `drive_c` count; each
/// source is sorted by path; duplicates are dropped.
///
/// On Linux, games outside Steam live in these launchers' prefixes.
pub fn launcher_prefixes(home: &Path, wine_prefix: Option<&Path>) -> Vec<WinePrefix> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut add = |mut paths: Vec<PathBuf>| {
        paths.sort();
        for path in paths {
            if path.join("drive_c").is_dir() && !found.contains(&path) {
                found.push(path);
            }
        }
    };
    add(wine_prefix.map(Path::to_path_buf).into_iter().collect());
    add(vec![home.join(".wine")]);
    let lutris_dirs = [
        ".config/lutris/games",
        ".local/share/lutris/games",
        ".var/app/net.lutris.Lutris/config/lutris/games",
        ".var/app/net.lutris.Lutris/data/lutris/games",
    ];
    add(lutris_dirs
        .iter()
        .flat_map(|dir| files_in(&home.join(dir), "yml"))
        .filter_map(|f| lutris_prefix(&f, home))
        .collect());
    let heroic_dirs = [".config/heroic/GamesConfig", ".var/app/com.heroicgameslauncher.hgl/config/heroic/GamesConfig"];
    add(heroic_dirs
        .iter()
        .flat_map(|dir| files_in(&home.join(dir), "json"))
        .flat_map(|f| heroic_prefixes(&f, home))
        .collect());
    add(subfolders(&home.join("Games/Heroic/Prefixes")));
    let bottles = [".local/share/bottles/bottles", ".var/app/com.usebottles.bottles/data/bottles/bottles"];
    add(bottles.iter().flat_map(|dir| subfolders(&home.join(dir))).collect());
    found.into_iter().map(WinePrefix::open).collect()
}

fn files_in(dir: &Path, extension: &str) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == extension) && path.is_file())
                .collect()
        })
        .unwrap_or_default()
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|entry| entry.path()).filter(|path| path.is_dir()).collect())
        .unwrap_or_default()
}

fn expand_home(value: &str, home: &Path) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if value == "~" => home.to_path_buf(),
        None => PathBuf::from(value),
    }
}

/// `game: prefix: <path>` of a Lutris game configuration (YAML, read line by line).
fn lutris_prefix(file: &Path, home: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let mut in_game = false;
    for line in text.lines() {
        if !line.starts_with([' ', '\t']) {
            in_game = line.trim_end() == "game:";
            continue;
        }
        if let Some(value) = line.trim_start().strip_prefix("prefix:").filter(|_| in_game) {
            let value = value.trim().trim_matches(|c| c == '\'' || c == '"');
            return (!value.is_empty()).then(|| expand_home(value, home));
        }
    }
    None
}

/// Every `winePrefix` of a Heroic game configuration (`{ "<app>": { "winePrefix": "…" } }`).
fn heroic_prefixes(file: &Path, home: &Path) -> Vec<PathBuf> {
    let Some(serde_json::Value::Object(apps)) =
        std::fs::read_to_string(file).ok().and_then(|text| serde_json::from_str(&text).ok())
    else {
        return Vec::new();
    };
    apps.values()
        .filter_map(|app| app.get("winePrefix")?.as_str())
        .filter(|value| !value.is_empty())
        .map(|value| expand_home(value, home))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Launcher prefixes: $WINEPREFIX, ~/.wine, Lutris, Heroic, Bottles; only folders with drive_c, no
    /// duplicates.
    #[test]
    fn launcher_prefix_sources() {
        let home = std::env::temp_dir().join(format!("openrigcue-launchers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let prefix = |path: &Path| fs::create_dir_all(path.join("drive_c")).unwrap();
        prefix(&home.join("custom"));
        prefix(&home.join(".wine"));
        prefix(&home.join("Games/dcs"));
        fs::create_dir_all(home.join(".config/lutris/games")).unwrap();
        fs::write(home.join(".config/lutris/games/dcs.yml"), "game:\n  exe: x\n  prefix: '~/Games/dcs'\nwine: {}\n")
            .unwrap();
        fs::write(home.join(".config/lutris/games/other.yml"), "system:\n  prefix: /nowhere\n").unwrap();
        prefix(&home.join("Games/Heroic/Prefixes/Condor"));
        fs::create_dir_all(home.join(".config/heroic/GamesConfig")).unwrap();
        fs::write(
            home.join(".config/heroic/GamesConfig/abc.json"),
            format!("{{\"abc\": {{\"winePrefix\": \"{}\"}}}}", home.join("custom").display()),
        )
        .unwrap();
        prefix(&home.join(".local/share/bottles/bottles/Games"));
        fs::create_dir_all(home.join(".local/share/bottles/bottles/broken")).unwrap();
        let roots: Vec<PathBuf> =
            launcher_prefixes(&home, Some(&home.join("custom"))).iter().map(|p| p.root().to_path_buf()).collect();
        assert_eq!(
            roots,
            [
                home.join("custom"),
                home.join(".wine"),
                home.join("Games/dcs"),
                home.join("Games/Heroic/Prefixes/Condor"),
                home.join(".local/share/bottles/bottles/Games"),
            ]
        );
        fs::create_dir_all(home.join(".wine/drive_c/users/alice/Documents")).unwrap();
        fs::create_dir_all(home.join(".wine/drive_c/users/Public")).unwrap();
        assert_eq!(WinePrefix::open(home.join(".wine")).documents(), home.join(".wine/drive_c/users/alice/Documents"));
        let _ = fs::remove_dir_all(&home);
    }

    /// SHGetFolderPath(CSIDL_PERSONAL) and %APPDATA% equivalents from the prefix registry and drives.
    #[test]
    fn user_folders_from_registry() {
        let root = std::env::temp_dir().join(format!("openrigcue-config-b-prefix-{}", std::process::id()));
        let users = root.join("drive_c/users/steamuser");
        fs::create_dir_all(users.join("Documents")).unwrap();
        fs::create_dir_all(root.join("dosdevices")).unwrap();
        fs::write(
            root.join("user.reg"),
            "[Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Explorer\\\\Shell Folders] 1\n\
             \"Personal\"=\"C:\\\\users\\\\steamuser\\\\Documents\"\n\
             \"AppData\"=\"C:\\\\users\\\\steamuser\\\\AppData\\\\Roaming\"\n",
        )
        .unwrap();
        let prefix = WinePrefix::open(root.clone());
        assert_eq!(prefix.documents(), users.join("Documents"));
        assert_eq!(prefix.app_data(), users.join("AppData/Roaming"));
        assert_eq!(prefix.local_app_data(), users.join("AppData/Local"));
        assert_eq!(prefix.profile(), users);
        assert_eq!(prefix.unix_path("D:\\Games"), Some(root.join("dosdevices/d:/Games")));
        assert_eq!(prefix.unix_path("relative"), None);
    }
}

//! The Proton bridge as part of a game's configuration.
//!
//! Some games publish telemetry through Windows shared memory or a named pipe.
//! Under Proton the game's Windows objects live inside its prefix, and
//! `openrigcue-bridge.exe` has to be started with the game. This module makes that part of the entry's ordinary
//! "config" action: the bridge files are copied to a stable folder the Steam runtime can see, and the game's
//! Steam launch options get `openrigcue-bridge-run` in front of `%command%`, with everything already in the
//! options kept.
//!
//! Only games whose listener reads a Windows section or pipe ([`BRIDGE_LISTENERS`]) and that are found in a
//! Steam library are touched. Steam keeps its launch options in memory and writes `localconfig.vdf` back while
//! it runs, so nothing is written while a `steam` process exists ([`BridgeError::SteamRunning`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use openrigcue_config::patch::{EntryKind, GameEntry, Patcher, SteamLibraries};

/// The launch wrapper and the bridge it starts.
pub const BRIDGE_RUN: &str = "openrigcue-bridge-run";
pub const BRIDGE_EXE: &str = "openrigcue-bridge.exe";
/// The bridge files, wrapper first.
pub const BRIDGE_FILES: [&str; 2] = [BRIDGE_RUN, BRIDGE_EXE];

/// Listener keys whose listener opens a Windows section or serves a named pipe (the listeners of
/// `openrigcue-bridge`'s catalog; `fs` reads the `FMTelemetry` pipe).
pub const BRIDGE_LISTENERS: &[&str] = &[
    "ac",
    "acevo",
    "projectcars",
    "projectcars2",
    "automobilista2",
    "iracing",
    "gpbikes",
    "mxbikes",
    "kart",
    "wrs",
    "gtr",
    "gtr2",
    "gtl",
    "race07",
    "trucksim",
    "falconbms",
    "rre",
    "rf1",
    "rfpro",
    "rf2",
    "trackmania",
    "spacemonkey",
    "warplanes",
    "il2cliffs",
    "fs",
];

/// How a Games-page entry's game is recognised in a Steam library. Entries that install a plugin through Steam
/// are found through their plugin location and need no rule here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeGame {
    /// Games-page entry key.
    pub entry: &'static str,
    /// Catalog id of the game the entry stands for (its listener is one of [`BRIDGE_LISTENERS`]).
    pub catalog: &'static str,
    /// The game's executable (the catalog's `process_name`), looked for in the app's folder.
    pub exes: &'static [&'static str],
    /// Steam folder names (`installdir`).
    pub dirs: &'static [&'static str],
    /// Steam application names, compared without case or punctuation.
    pub names: &'static [&'static str],
}

const fn game(
    entry: &'static str,
    catalog: &'static str,
    exes: &'static [&'static str],
    dirs: &'static [&'static str],
    names: &'static [&'static str],
) -> BridgeGame {
    BridgeGame { entry, catalog, exes, dirs, names }
}

/// Every PC entry whose game needs the bridge under Proton.
pub const BRIDGE_GAMES: &[BridgeGame] = &[
    game("ac", "ac", &["acs.exe"], &[], &["Assetto Corsa"]),
    game("acc", "acc", &["acc.exe"], &[], &["Assetto Corsa Competizione"]),
    game("acevo", "acevo", &["AssettoCorsaEVO.exe"], &[], &["Assetto Corsa EVO"]),
    game("acrally", "acrally", &["acr.exe"], &[], &["Assetto Corsa Rally"]),
    game("automobilista2", "automobilista2", &["AMS2AVX.exe"], &[], &["Automobilista 2"]),
    game("projectcars", "projectcars", &["pCARS64.exe"], &[], &["Project CARS"]),
    game("projectcars2", "projectcars2", &["pCARS2AVX.exe"], &[], &["Project CARS 2"]),
    game("projectcars3", "projectcars3", &["pCARS3.exe"], &[], &["Project CARS 3"]),
    game("iracing", "iracing", &["iRacingSim64DX11.exe"], &[], &["iRacing"]),
    game("kart", "kart", &["kart.exe"], &[], &["Kart Racing Pro"]),
    game("mxbikes", "mxbikes", &["mxbikes.exe"], &[], &["MX Bikes"]),
    game("gpbikes", "gpbikes", &[], &[], &["GP Bikes"]),
    game("wrs", "wrs", &[], &[], &["World Racing Series"]),
    game("gtr", "gtr", &[], &[], &["GTR - FIA GT Racing Game"]),
    game("gtr2", "gtr2", &[], &[], &["GTR 2 - FIA GT Racing Game"]),
    game("gtl", "gtl", &[], &[], &["GT Legends"]),
    game("race07", "race07", &[], &[], &["RACE 07"]),
    game("raceinjection", "raceinjection", &[], &[], &["RACE Injection"]),
    game("volvo", "volvo", &[], &[], &["Volvo - The Game"]),
    game("wtcc", "wtcc", &["Race_Steam.exe"], &[], &[]),
    game("falconbms", "falconbms", &["Falcon BMS.exe"], &[], &[]),
    game("rre", "rre", &[], &[], &["RaceRoom Racing Experience"]),
    game("rrevo", "rrevo", &[], &[], &["RaceRoom EvO"]),
    game("trackmania2", "trackmania2", &["Trackmania2.exe"], &[], &[]),
    game("trackmaniaturbo", "trackmaniaturbo", &["TrackmaniaTurbo.exe"], &[], &["Trackmania Turbo"]),
    // `Launcher64.exe` is too common a file name to identify the game.
    game(
        "il2_cliffs",
        "il2_cliffs",
        &[],
        &[],
        &["IL-2 Sturmovik: Cliffs of Dover", "IL-2 Sturmovik: Cliffs of Dover Blitz Edition"],
    ),
    game("fs19", "fs19", &[], &[], &["Farming Simulator 19"]),
    game("fs22", "fs22", &[], &[], &["Farming Simulator 22"]),
    game("fs25", "fs25", &[], &[], &["Farming Simulator 25"]),
    // Found through their Steam plugin location.
    game("rf1", "rf1", &[], &[], &[]),
    game("rf1_demo", "rf1", &[], &[], &[]),
    game("automobilista", "automobilista", &[], &[], &[]),
    game("actc", "actc", &[], &[], &[]),
    game("arca", "arca", &[], &[], &[]),
    game("ftruck2013", "ftruck2013", &[], &[], &[]),
    game("gsc2012", "gsc2012", &[], &[], &[]),
    game("marcas", "marcas", &[], &[], &[]),
    game("simraceway", "simraceway", &[], &[], &[]),
    game("ats", "ats", &[], &[], &[]),
    game("ets", "ets", &[], &[], &[]),
    game("warplanes_ww1", "warplanes_ww1", &[], &[], &[]),
    game("warplanes_bop", "warplanes_bop", &[], &[], &[]),
    // The rFactor 2 plugin locations.
    game("rf2", "rf2", &[], &["rFactor 2"], &[]),
    game("lemans", "lemans", &[], &["Le Mans Ultimate"], &[]),
];

/// PC entries of bridge listeners that are not Steam games: nothing can be set up for them here.
/// `projectcarspro` is the arcade edition, `rfpro` the professional simulator, `spacemonkey` a telemetry
/// provider program rather than a game.
pub const NOT_STEAM_GAMES: &[&str] = &["projectcarspro", "rfpro", "spacemonkey"];

/// The bridge rule of a Games-page entry.
pub fn bridge_game(entry: &str) -> Option<&'static BridgeGame> {
    BRIDGE_GAMES.iter().find(|game| game.entry == entry)
}

/// Whether the game of a Games-page entry only sends telemetry when it is started through the bridge.
pub fn needs_bridge(entry: &str) -> bool {
    bridge_game(entry).is_some() || NOT_STEAM_GAMES.contains(&entry)
}

/// Where the bridge and its launch wrapper are for a game that a launcher other than Steam starts (what
/// `openrigcue bridge-path` prints).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeLaunch {
    /// `openrigcue-bridge.exe`.
    pub bridge: PathBuf,
    /// Whether `bridge` is there.
    pub bridge_present: bool,
    /// The launch wrapper.
    pub run: PathBuf,
}

impl BridgeLaunch {
    /// The launch option that starts the game through the wrapper.
    pub fn option(&self) -> String {
        format!("\"{}\" %command%", self.run.display())
    }
}

/// The bridge of the first folder of `candidates` that holds the launch wrapper, `None` when none does. With
/// `stable` (a program whose own folder changes on every start) the files are first copied there and those
/// copies are named.
pub fn bridge_launch(candidates: &[PathBuf], stable: Option<&Path>) -> io::Result<Option<BridgeLaunch>> {
    let Some(found) = candidates.iter().find(|dir| dir.join(BRIDGE_RUN).is_file()) else {
        return Ok(None);
    };
    let dir = match stable {
        Some(stable) => {
            install(found, stable)?;
            stable.to_path_buf()
        }
        None => found.canonicalize()?,
    };
    let bridge = dir.join(BRIDGE_EXE);
    Ok(Some(BridgeLaunch { bridge_present: bridge.is_file(), bridge, run: dir.join(BRIDGE_RUN) }))
}

/// Why the bridge part of a game's configuration was not applied.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// Steam would overwrite the launch options with its own copy.
    #[error("Steam is running")]
    SteamRunning,
    #[error("the bridge files are missing")]
    FilesMissing,
    /// `localconfig.vdf` is not the text this module can edit safely; it is left untouched.
    #[error("{} could not be read as Steam's configuration", .0.display())]
    Unreadable(PathBuf),
    /// The file changed between reading and writing it; it is left as the other writer left it.
    #[error("{} changed while it was being configured", .0.display())]
    Changed(PathBuf),
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
}

/// Where the bridge files come from and go to, and where running processes are listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSetup {
    /// Folder holding the packaged bridge files, when one was found.
    source: Option<PathBuf>,
    /// The user's data folder (`$XDG_DATA_HOME`), which the Steam runtime container shows to the game.
    data: PathBuf,
    home: PathBuf,
    proc_root: PathBuf,
    enabled: bool,
}

impl BridgeSetup {
    /// The running program's bridge: its packaged files, the user's data folder and `/proc`.
    pub fn for_service(home: &Path) -> BridgeSetup {
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        BridgeSetup {
            source: packaged_dirs().into_iter().find(|dir| BRIDGE_FILES.iter().all(|name| dir.join(name).is_file())),
            data,
            home: home.to_path_buf(),
            proc_root: PathBuf::from("/proc"),
            enabled: true,
        }
    }

    /// No bridge set-up: every entry keeps its plain status and action.
    pub fn disabled(home: &Path) -> BridgeSetup {
        BridgeSetup {
            source: None,
            data: home.join(".local/share"),
            home: home.to_path_buf(),
            proc_root: PathBuf::from("/proc"),
            enabled: false,
        }
    }

    /// Explicit locations (tests and tools): `source` holds the packaged files, `data` stands for
    /// `$XDG_DATA_HOME`, `proc_root` for `/proc`.
    pub fn with_locations(home: &Path, source: Option<&Path>, data: &Path, proc_root: &Path) -> BridgeSetup {
        BridgeSetup {
            source: source.map(Path::to_path_buf),
            data: data.to_path_buf(),
            home: home.to_path_buf(),
            proc_root: proc_root.to_path_buf(),
            enabled: true,
        }
    }

    /// The stable folder the bridge files are copied to for a Steam installation. A Flatpak Steam only sees its
    /// own `~/.var/app/<id>` folder, so the files go there.
    fn deploy_dir(&self, steam_root: &Path) -> PathBuf {
        let flatpak_apps = self.home.join(".var/app");
        if let Ok(inside) = steam_root.strip_prefix(&flatpak_apps) {
            if let Some(app) = inside.components().next() {
                return flatpak_apps.join(app).join("data/openrigcue/bridge");
            }
        }
        self.data.join("openrigcue").join("bridge")
    }

    /// What is installed in Steam and how its launch options stand, read now. `None` when there is nothing this
    /// program could set up: no Steam, no Steam user, or no bridge files.
    pub fn view(&self, steam: Option<&SteamLibraries>) -> Option<SteamView> {
        if !self.enabled {
            return None;
        }
        let steam = steam?;
        let root = steam.library_folders.first()?;
        let deploy = self.deploy_dir(root);
        let deployed = BRIDGE_FILES.iter().all(|name| deploy.join(name).is_file());
        let current = match &self.source {
            Some(source) => BRIDGE_FILES.iter().all(|name| same_file(&source.join(name), &deploy.join(name))),
            None => deployed,
        };
        if self.source.is_none() && !deployed {
            return None;
        }
        let configs: Vec<LocalConfig> = user_configs(root)
            .into_iter()
            .map(|path| {
                let text = fs::read(&path).ok().and_then(|bytes| String::from_utf8(bytes).ok());
                LocalConfig { path, text }
            })
            .collect();
        if configs.is_empty() {
            return None;
        }
        Some(SteamView { apps: installed_apps(steam), configs, run: deploy.join(BRIDGE_RUN), current })
    }

    /// Brings a stable folder that already holds the bridge up to this program's files (after an update). A
    /// folder that was never set up is left alone, and Steam's own files are not touched.
    pub fn refresh(&self, steam: Option<&SteamLibraries>) {
        let (Some(source), Some(root)) = (&self.source, steam.and_then(|steam| steam.library_folders.first())) else {
            return;
        };
        let deploy = self.deploy_dir(root);
        if self.enabled && deploy.join(BRIDGE_RUN).is_file() {
            if let Err(error) = install(source, &deploy) {
                tracing::warn!(folder = %deploy.display(), %error, "updating the Proton bridge files failed");
            }
        }
    }

    /// The bridge of the running program for a launcher other than Steam ([`bridge_launch`]). Run from the
    /// AppImage, whose mount point changes on every start, the files are first copied to the user's data folder.
    /// `None` when the packaged files are not found, and for a set-up without bridge.
    pub fn launch(&self) -> io::Result<Option<BridgeLaunch>> {
        if !self.enabled {
            return Ok(None);
        }
        // Without a folder that holds both files, one that holds the wrapper alone still names where the
        // bridge belongs.
        let candidates = self.source.clone().map_or_else(packaged_dirs, |source| vec![source]);
        let stable = std::env::var_os("APPIMAGE").map(|_| self.data.join("openrigcue").join("bridge"));
        bridge_launch(&candidates, stable.as_deref())
    }

    /// Whether a Steam client is running: a process named `steam`. An unreadable process list counts as running,
    /// so that nothing is written blind.
    pub fn steam_running(&self) -> bool {
        let Ok(entries) = fs::read_dir(&self.proc_root) else {
            return true;
        };
        entries.flatten().any(|entry| {
            entry.file_name().to_str().is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
                && fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim_end() == "steam")
        })
    }

    /// Copies the bridge files to their stable folder and puts the wrapper in front of `%command%` in the app's
    /// launch options of every Steam user, keeping what the options already hold. The files are copied whether or
    /// not Steam runs; the launch options are only written while it does not.
    pub fn configure(&self, steam: &SteamLibraries, app: &SteamApp) -> Result<(), BridgeError> {
        let root = steam.library_folders.first().ok_or(BridgeError::FilesMissing)?;
        let deploy = self.deploy_dir(root);
        match &self.source {
            Some(source) => install(source, &deploy)?,
            None if BRIDGE_FILES.iter().all(|name| deploy.join(name).is_file()) => {}
            None => return Err(BridgeError::FilesMissing),
        }
        let run = deploy.join(BRIDGE_RUN);
        for path in user_configs(root) {
            let bytes = fs::read(&path)?;
            let text = String::from_utf8(bytes.clone()).map_err(|_| BridgeError::Unreadable(path.clone()))?;
            let options = match launch_options(&text, &app.id) {
                Lookup::Value { value, .. } => value,
                Lookup::Missing { .. } => String::new(),
                Lookup::Malformed => return Err(BridgeError::Unreadable(path)),
            };
            if wrapper_in(&options, &run).is_some() {
                continue;
            }
            if self.steam_running() {
                return Err(BridgeError::SteamRunning);
            }
            let changed = set_launch_options(&text, &app.id, &with_bridge(&options, &run))
                .ok_or_else(|| BridgeError::Unreadable(path.clone()))?;
            self.replace(&path, &bytes, changed.as_bytes())?;
        }
        Ok(())
    }

    /// Replaces `path` with `after` when it still holds `before`: the first change keeps the user's file as
    /// `localconfigsrs_bck.vdf`, the new text is written next to the file and renamed over it.
    fn replace(&self, path: &Path, before: &[u8], after: &[u8]) -> Result<(), BridgeError> {
        let backup = path.with_file_name("localconfigsrs_bck.vdf");
        if !backup.exists() {
            fs::write(&backup, before)?;
        }
        let temporary = path.with_file_name("localconfig.vdf.openrigcue");
        fs::write(&temporary, after)?;
        let finish = || -> Result<(), BridgeError> {
            fs::set_permissions(&temporary, fs::metadata(path)?.permissions())?;
            if self.steam_running() {
                return Err(BridgeError::SteamRunning);
            }
            if fs::read(path)? != before {
                return Err(BridgeError::Changed(path.to_path_buf()));
            }
            Ok(fs::rename(&temporary, path)?)
        };
        finish().inspect_err(|_| {
            let _ = fs::remove_file(&temporary);
        })
    }
}

/// Folders searched for the packaged bridge files: `../lib/openrigcue` of this executable, its own folder, and
/// the AppImage's `usr/lib/openrigcue` and `usr/bin`.
pub fn packaged_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        dirs.push(dir.join("../lib/openrigcue"));
        dirs.push(dir);
    }
    if let Some(appdir) = std::env::var_os("APPDIR").map(PathBuf::from) {
        dirs.push(appdir.join("usr/lib/openrigcue"));
        dirs.push(appdir.join("usr/bin"));
    }
    dirs
}

/// Copies the bridge files of `from` that differ into `to`, the wrapper executable.
pub fn install(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for name in BRIDGE_FILES {
        let source = from.join(name);
        let target = to.join(name);
        if source.is_file() && !same_file(&source, &target) {
            // Written beside the target and renamed, so a running game keeps the file it started with.
            let temporary = to.join(format!("{name}.new"));
            fs::copy(&source, &temporary)?;
            fs::rename(&temporary, &target)?;
        }
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(left), Ok(right)) if left.len() == right.len() => {
            matches!((fs::read(a), fs::read(b)), (Ok(left), Ok(right)) if left == right)
        }
        _ => false,
    }
}

/// `userdata/<account>/config/localconfig.vdf` of every Steam user of the installation, by account number.
fn user_configs(steam_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(steam_root.join("userdata")) else {
        return Vec::new();
    };
    let mut users: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let account: u64 = entry.file_name().to_str()?.parse().ok()?;
            Some((account, entry.path().join("config").join("localconfig.vdf")))
        })
        .filter(|(account, path)| *account != 0 && path.is_file())
        .collect();
    users.sort();
    users.into_iter().map(|(_, path)| path).collect()
}

/// An application of a Steam library (`steamapps/appmanifest_<id>.acf`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamApp {
    pub id: String,
    pub name: String,
    /// `steamapps/common/<installdir>` of its library.
    pub dir: PathBuf,
    /// The executables of [`BRIDGE_GAMES`] found in its folder.
    exes: Vec<&'static str>,
}

fn installed_apps(steam: &SteamLibraries) -> Vec<SteamApp> {
    let mut apps = Vec::new();
    for library in &steam.library_folders {
        let steamapps = library.join("steamapps");
        let Ok(entries) = fs::read_dir(&steamapps) else {
            continue;
        };
        let mut manifests: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("appmanifest_") && name.ends_with(".acf"))
            })
            .collect();
        manifests.sort();
        for manifest in manifests {
            let Some(text) = fs::read(&manifest).ok().and_then(|bytes| String::from_utf8(bytes).ok()) else {
                continue;
            };
            let field = |key: &str| match lookup(&text, &["AppState", key]) {
                Lookup::Value { value, .. } => Some(value),
                _ => None,
            };
            let (Some(id), Some(installdir)) = (field("appid"), field("installdir")) else {
                continue;
            };
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || installdir.is_empty() {
                continue;
            }
            let dir = steamapps.join("common").join(installdir);
            if dir.is_dir() && !apps.iter().any(|known: &SteamApp| known.id == id) {
                let exes = rule_executables(&dir);
                apps.push(SteamApp { id, name: field("name").unwrap_or_default(), dir, exes });
            }
        }
    }
    apps
}

/// One Steam user's `localconfig.vdf`; `text` is `None` when it could not be read as UTF-8 text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalConfig {
    path: PathBuf,
    text: Option<String>,
}

/// Steam's installed applications and launch options at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamView {
    apps: Vec<SteamApp>,
    configs: Vec<LocalConfig>,
    /// The wrapper in its stable folder.
    run: PathBuf,
    /// The stable folder holds the packaged files.
    current: bool,
}

impl SteamView {
    /// The Steam application of a Games-page entry: `None` for an entry that needs no bridge and for a game that
    /// is not in a Steam library.
    pub fn app_for(&self, entry: &GameEntry, steam: &SteamLibraries) -> Option<&SteamApp> {
        let rule = bridge_game(entry.key)?;
        if let EntryKind::Dll(Patcher::SteamPlugin(plugin)) | EntryKind::Config(Patcher::SteamPlugin(plugin)) =
            entry.kind
        {
            let found = steam.find_game(plugin.location)?;
            return self.apps.iter().find(|app| is_within(&found, &app.dir));
        }
        self.apps
            .iter()
            .find(|app| {
                app.dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| rule.dirs.iter().any(|dir| dir.eq_ignore_ascii_case(name)))
            })
            .or_else(|| self.apps.iter().find(|app| rule.exes.iter().any(|exe| app.exes.contains(exe))))
            .or_else(|| {
                self.apps.iter().find(|app| rule.names.iter().any(|name| plain_name(name) == plain_name(&app.name)))
            })
    }

    /// The bridge starts with the app: the stable folder is up to date and every Steam user's launch options run
    /// the wrapper in front of `%command%`.
    pub fn configured(&self, app: &SteamApp) -> bool {
        self.configs.iter().all(|config| {
            let Some(text) = &config.text else {
                return false;
            };
            let Lookup::Value { value, .. } = launch_options(text, &app.id) else {
                return false;
            };
            wrapper_in(&value, &self.run).is_some_and(|wrapper| wrapper != self.run || self.current)
        })
    }
}

/// A Steam name without case, punctuation or trademark signs.
fn plain_name(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// `path` is `dir` or lies below it; Windows names are matched without case.
fn is_within(path: &Path, dir: &Path) -> bool {
    let (path, dir) = (path.to_string_lossy().to_lowercase(), dir.to_string_lossy().to_lowercase());
    path.strip_prefix(dir.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Directory entries looked at when searching an app's folder for a game executable.
const SEARCH_LIMIT: usize = 20_000;

/// The executables of [`BRIDGE_GAMES`] (whatever their case) in `dir` or up to two folders below it.
fn rule_executables(dir: &Path) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut seen = 0;
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..3 {
        let mut next = Vec::new();
        for folder in &level {
            let Ok(entries) = fs::read_dir(folder) else {
                continue;
            };
            for entry in entries.flatten() {
                seen += 1;
                if seen > SEARCH_LIMIT {
                    return found;
                }
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    next.push(entry.path());
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let rule = BRIDGE_GAMES.iter().flat_map(|game| game.exes).find(|exe| exe.eq_ignore_ascii_case(name));
                if let Some(exe) = rule.filter(|exe| !found.contains(*exe)) {
                    found.push(*exe);
                }
            }
        }
        level = next;
    }
    found
}

// --- Valve KeyValues text, edited in place -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A key or value: its bytes in the text (quotes included) and its unescaped text.
    Word {
        start: usize,
        end: usize,
        value: String,
    },
    Open,
    /// A closing brace and its position.
    Close(usize),
}

fn tokens(text: &str) -> Option<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                out.push(Token::Open);
                i += 1;
            }
            b'}' => {
                out.push(Token::Close(i));
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut value = Vec::new();
                loop {
                    match *bytes.get(i)? {
                        b'\\' => {
                            match *bytes.get(i + 1)? {
                                b'n' => value.push(b'\n'),
                                b't' => value.push(b'\t'),
                                escaped @ (b'\\' | b'"') => value.push(escaped),
                                other => value.extend([b'\\', other]),
                            }
                            i += 2;
                        }
                        b'"' => {
                            i += 1;
                            break;
                        }
                        other => {
                            value.push(other);
                            i += 1;
                        }
                    }
                }
                out.push(Token::Word { start, end: i, value: String::from_utf8(value).ok()? });
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b if b.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && !matches!(bytes[i], b'{' | b'}' | b'"') {
                    i += 1;
                }
                out.push(Token::Word { start, end: i, value: text[start..i].to_owned() });
            }
        }
    }
    Some(out)
}

/// Where a key path leads in a KeyValues text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lookup {
    /// The value and the bytes it occupies.
    Value { start: usize, end: usize, value: String },
    /// The last `missing` keys of the path do not exist; `close` is the closing brace of the deepest section that
    /// does.
    Missing { missing: usize, close: usize },
    /// Not KeyValues text, or the path's first key is not there.
    Malformed,
}

/// Finds `path` (keys compared without case, as Steam does; the first match at each level counts).
fn lookup(text: &str, path: &[&str]) -> Lookup {
    let Some(tokens) = tokens(text) else {
        return Lookup::Malformed;
    };
    let mut position = 0;
    match section(&tokens, &mut position, path, false) {
        Some((Some(found), _)) => found,
        _ => Lookup::Malformed,
    }
}

/// Reads the entries of one section; the result is what `path` found in it and the section's closing brace.
fn section(tokens: &[Token], position: &mut usize, path: &[&str], nested: bool) -> Option<(Option<Lookup>, usize)> {
    let mut found = None;
    loop {
        let token = tokens.get(*position);
        *position += 1;
        let key = match token {
            None => return (!nested).then_some((found, 0)),
            Some(Token::Close(at)) => return nested.then_some((found, *at)),
            Some(Token::Open) => return None,
            Some(Token::Word { value, .. }) => value,
        };
        let wanted = found.is_none() && path.first().is_some_and(|name| name.eq_ignore_ascii_case(key));
        let value = tokens.get(*position)?;
        *position += 1;
        match value {
            Token::Word { start, end, value } => {
                if wanted && path.len() == 1 {
                    found = Some(Lookup::Value { start: *start, end: *end, value: value.clone() });
                }
            }
            Token::Open => {
                let inner_path: &[&str] = if wanted && path.len() > 1 { &path[1..] } else { &[] };
                let (inner, close) = section(tokens, position, inner_path, true)?;
                if wanted && path.len() > 1 {
                    found = Some(inner.unwrap_or(Lookup::Missing { missing: path.len() - 1, close }));
                }
            }
            Token::Close(_) => return None,
        }
    }
}

fn launch_path(app: &str) -> [&str; 7] {
    ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", app, "LaunchOptions"]
}

/// `UserLocalConfigStore/Software/Valve/Steam/apps/<app>/LaunchOptions` of a `localconfig.vdf`.
fn launch_options(text: &str, app: &str) -> Lookup {
    lookup(text, &launch_path(app))
}

fn escaped(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The text with the app's launch options set to `options`; every other byte stays. Missing sections of the path
/// are added in Steam's layout. `None` when the text cannot be edited safely.
fn set_launch_options(text: &str, app: &str, options: &str) -> Option<String> {
    let path = launch_path(app);
    match lookup(text, &path) {
        Lookup::Value { start, end, .. } => Some(format!("{}\"{}\"{}", &text[..start], escaped(options), &text[end..])),
        Lookup::Missing { missing, close } => {
            let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
            let depth = path.len() - missing;
            let mut added = String::new();
            for (offset, key) in path[depth..path.len() - 1].iter().enumerate() {
                let indent = "\t".repeat(depth + offset);
                added.push_str(&format!("{indent}\"{}\"{newline}{indent}{{{newline}", escaped(key)));
            }
            let indent = "\t".repeat(path.len() - 1);
            added.push_str(&format!("{indent}\"LaunchOptions\"\t\t\"{}\"{newline}", escaped(options)));
            for level in (depth..path.len() - 1).rev() {
                added.push_str(&format!("{}}}{newline}", "\t".repeat(level)));
            }
            // In front of the line that closes the section, when the brace has that line to itself.
            let line = text[..close].rfind('\n').map_or(0, |at| at + 1);
            let at = if text[line..close].bytes().all(|b| b == b'\t' || b == b' ') { line } else { close };
            Some(format!("{}{added}{}", &text[..at], &text[at..]))
        }
        Lookup::Malformed => None,
    }
}

// --- Launch options ------------------------------------------------------------------------------------------

/// A word of a launch-option line as a shell reads it: its bytes in the line and its unquoted text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    start: usize,
    end: usize,
    value: String,
}

fn words(line: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut value = String::new();
        let mut end = line.len();
        let mut quote = None;
        while let Some(&(at, c)) = chars.peek() {
            match (quote, c) {
                (None, c) if c.is_whitespace() => {
                    end = at;
                    break;
                }
                (None, '\'' | '"') => quote = Some(c),
                (Some(open), c) if c == open => quote = None,
                (None | Some('"'), '\\') => {
                    chars.next();
                    match chars.peek() {
                        Some(&(_, next)) if quote.is_none() || matches!(next, '"' | '\\' | '$' | '`') => {
                            value.push(next);
                        }
                        Some(&(_, next)) => value.extend(['\\', next]),
                        None => {
                            value.push('\\');
                            break;
                        }
                    }
                }
                (_, c) => value.push(c),
            }
            chars.next();
        }
        out.push(Word { start, end, value });
    }
    out
}

fn is_wrapper(word: &str) -> bool {
    Path::new(word).file_name().is_some_and(|name| name == BRIDGE_RUN)
}

/// The wrapper the launch options start in front of `%command%`, when it is `run` or another existing file.
fn wrapper_in(options: &str, run: &Path) -> Option<PathBuf> {
    let words = words(options);
    let command = words.iter().position(|word| options[word.start..word.end].contains("%command%"))?;
    let wrapper = words[..command].iter().find(|word| is_wrapper(&word.value))?;
    let path = PathBuf::from(&wrapper.value);
    (path == run || path.is_file()).then_some(path)
}

fn shell_quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "_./+-@:,=".contains(c)) {
        return text.into_owned();
    }
    let mut quoted = String::from("\"");
    for c in text.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// The launch options with `run` in front of `%command%`. Everything else in the options stays as it is: words
/// in front of `%command%` (environment, other wrappers) still run first, and options without `%command%` are
/// the game's arguments, which Steam appends, so they follow `%command%`. An earlier wrapper word is replaced.
pub fn with_bridge(options: &str, run: &Path) -> String {
    let mut rest = String::new();
    let mut from = 0;
    for word in words(options).iter().filter(|word| is_wrapper(&word.value)) {
        rest.push_str(&options[from..word.start]);
        from = word.end + options[word.end..].len() - options[word.end..].trim_start().len();
    }
    rest.push_str(&options[from..]);
    let quoted = shell_quoted(run);
    match words(&rest).iter().find(|word| rest[word.start..word.end].contains("%command%")) {
        Some(command) => format!("{}{quoted} {}", &rest[..command.start], &rest[command.start..]),
        None if rest.trim().is_empty() => format!("{quoted} %command%"),
        None => format!("{quoted} %command% {}", rest.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrigcue_config::catalog::Catalog;
    use openrigcue_config::patch::GAME_ENTRIES;

    const RUN: &str = "/home/u/.local/share/openrigcue/bridge/openrigcue-bridge-run";

    /// The listener list is the bridge's own catalog (`sections("<key>", …)` of its `LISTENERS`).
    #[test]
    fn listeners_are_the_bridge_catalog() {
        let source = include_str!("../../../openrigcue-bridge/src/catalog.rs");
        let table = &source[source.find("pub const LISTENERS").unwrap()..];
        let table = &table[..table.find("\n];").unwrap()];
        let mut keys = Vec::new();
        for part in table.split("sections(").skip(1) {
            let part = part.trim_start();
            keys.push(part[1..].split('"').next().unwrap());
        }
        assert_eq!(keys, BRIDGE_LISTENERS);
    }

    /// Every rule names a PC entry and a catalog game of a bridge listener, with the catalog's process name;
    /// every PC entry of such a game has a rule or is listed as not a Steam game.
    #[test]
    fn rules_cover_the_games_page() {
        let catalog = Catalog::builtin();
        for rule in BRIDGE_GAMES {
            let entry = GAME_ENTRIES.iter().find(|entry| entry.key == rule.entry).expect(rule.entry);
            assert!(entry.platforms.pc, "{}", rule.entry);
            let game = catalog.game(rule.catalog).expect(rule.catalog);
            assert!(BRIDGE_LISTENERS.contains(&game.listener.as_str()), "{}", rule.entry);
            if !rule.exes.is_empty() {
                assert_eq!(game.process_name.as_deref(), Some(rule.exes[0]), "{}", rule.entry);
                assert_eq!(rule.exes.len(), 1);
            }
            assert_eq!(BRIDGE_GAMES.iter().filter(|other| other.entry == rule.entry).count(), 1);
        }
        for entry in GAME_ENTRIES.iter().filter(|entry| entry.platforms.pc) {
            let needs = [entry.key, entry.catalog_id()]
                .iter()
                .filter_map(|id| catalog.game(id))
                .any(|game| BRIDGE_LISTENERS.contains(&game.listener.as_str()));
            let ruled = bridge_game(entry.key).is_some();
            let excluded = NOT_STEAM_GAMES.contains(&entry.key);
            assert!(!(ruled && excluded), "{}", entry.key);
            if entry.key == "il2" || entry.key == "il2_steam" {
                // The part of `il2_cliffs` before its first `_` is `il2`; these two are the UDP game.
                assert!(!ruled);
                continue;
            }
            assert_eq!(needs, ruled || excluded, "{}", entry.key);
        }
    }

    #[test]
    fn launch_options_keep_what_they_hold() {
        let run = Path::new(RUN);
        assert_eq!(with_bridge("", run), format!("{RUN} %command%"));
        assert_eq!(with_bridge("  ", run), format!("{RUN} %command%"));
        assert_eq!(with_bridge("-novid -w 1920", run), format!("{RUN} %command% -novid -w 1920"));
        assert_eq!(
            with_bridge("PROTON_LOG=1 gamemoderun %command% -vr", run),
            format!("PROTON_LOG=1 gamemoderun {RUN} %command% -vr")
        );
        assert_eq!(
            with_bridge("gamescope -W 2560 -- \"%command%\"", run),
            format!("gamescope -W 2560 -- {RUN} \"%command%\"")
        );
        // An earlier wrapper (an AppImage mount that is gone) is replaced, the rest stays.
        assert_eq!(
            with_bridge("mangohud \"/tmp/.mount_x/usr/lib/openrigcue/openrigcue-bridge-run\" %command% -x", run),
            format!("mangohud {RUN} %command% -x")
        );
        assert_eq!(with_bridge("/old/openrigcue-bridge-run", run), format!("{RUN} %command%"));
        // Applying it again changes nothing.
        let once = with_bridge("A=1 %command% -b", run);
        assert_eq!(with_bridge(&once, run), once);
        let spaced = Path::new("/home/a b/openrigcue-bridge-run");
        assert_eq!(with_bridge("", spaced), "\"/home/a b/openrigcue-bridge-run\" %command%");
        assert_eq!(wrapper_in(&with_bridge("x=1 %command%", spaced), spaced).as_deref(), Some(spaced));
    }

    #[test]
    fn wrapper_must_precede_the_command_and_exist() {
        let run = Path::new(RUN);
        assert_eq!(wrapper_in(&format!("{RUN} %command%"), run).as_deref(), Some(run));
        assert_eq!(wrapper_in(&format!("A=1 '{RUN}' %command% -x"), run).as_deref(), Some(run));
        assert_eq!(wrapper_in(&format!("%command% {RUN}"), run), None);
        assert_eq!(wrapper_in(RUN, run), None);
        assert_eq!(wrapper_in("/gone/openrigcue-bridge-run %command%", run), None);
        assert_eq!(wrapper_in("gamemoderun %command%", run), None);
        let directory = tempfile::tempdir().unwrap();
        let other = directory.path().join(BRIDGE_RUN);
        fs::write(&other, "").unwrap();
        assert_eq!(wrapper_in(&format!("{} %command%", other.display()), run), Some(other));
    }

    const LOCALCONFIG: &str = "\"UserLocalConfigStore\"\n{\n\t\"Broadcast\"\n\t{\n\t\t\"Permissions\"\t\t\"1\"\n\t}\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"244210\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1794000000\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"WINEDLLOVERRIDES=\\\"dinput8=n,b\\\" %command% -x\"\n\t\t\t\t\t}\n\t\t\t\t\t\"805550\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t\t\"LastPlayedTimesSyncTime\"\t\t\"7\"\n\t\t\t}\n\t\t}\n\t}\n\t\"WebStorage\"\n\t{\n\t\t\"x\"\t\t\"{\\\"a\\\":\\\"b}\\\"}\"\n\t}\n}\n";

    #[test]
    fn existing_value_is_replaced_in_place() {
        let Lookup::Value { value, .. } = launch_options(LOCALCONFIG, "244210") else { panic!() };
        assert_eq!(value, "WINEDLLOVERRIDES=\"dinput8=n,b\" %command% -x");
        let new = with_bridge(&value, Path::new(RUN));
        let changed = set_launch_options(LOCALCONFIG, "244210", &new).unwrap();
        let line = format!("\"LaunchOptions\"\t\t\"WINEDLLOVERRIDES=\\\"dinput8=n,b\\\" {RUN} %command% -x\"");
        assert_eq!(
            changed,
            LOCALCONFIG.replace("\"LaunchOptions\"\t\t\"WINEDLLOVERRIDES=\\\"dinput8=n,b\\\" %command% -x\"", &line)
        );
        let Lookup::Value { value, .. } = launch_options(&changed, "244210") else { panic!() };
        assert_eq!(value, new);
    }

    #[test]
    fn missing_key_and_missing_app_are_added_in_steams_layout() {
        let options = format!("{RUN} %command%");
        // The app's section exists without the key.
        assert!(matches!(launch_options(LOCALCONFIG, "805550"), Lookup::Missing { missing: 1, .. }));
        let changed = set_launch_options(LOCALCONFIG, "805550", &options).unwrap();
        assert_eq!(
            changed,
            LOCALCONFIG.replace(
                "\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n",
                &format!("\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{options}\"\n")
            )
        );
        // No section for the app yet.
        assert!(matches!(launch_options(LOCALCONFIG, "1066890"), Lookup::Missing { missing: 2, .. }));
        let changed = set_launch_options(LOCALCONFIG, "1066890", &options).unwrap();
        let added = format!(
            "\t\t\t\t\t\"1066890\"\n\t\t\t\t\t{{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"{options}\"\n\t\t\t\t\t}}\n"
        );
        assert_eq!(
            changed,
            LOCALCONFIG.replace(
                "\t\t\t\t}\n\t\t\t\t\"LastPlayedTimesSyncTime\"",
                &format!("{added}\t\t\t\t}}\n\t\t\t\t\"LastPlayedTimesSyncTime\"")
            )
        );
        let Lookup::Value { value, .. } = launch_options(&changed, "1066890") else { panic!() };
        assert_eq!(value, options);
        // Keys match without case, as in Steam's own files ("Apps", "software").
        let cased = LOCALCONFIG.replace("\"apps\"", "\"Apps\"").replace("\"Software\"", "\"software\"");
        assert!(matches!(launch_options(&cased, "244210"), Lookup::Value { .. }));
    }

    /// A file another writer changed after it was read is left as that writer left it.
    #[test]
    fn a_file_changed_meanwhile_is_not_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let proc_root = directory.path().join("proc");
        fs::create_dir_all(&proc_root).unwrap();
        let setup = BridgeSetup::with_locations(directory.path(), None, directory.path(), &proc_root);
        let path = directory.path().join("localconfig.vdf");
        fs::write(&path, "theirs").unwrap();
        assert!(matches!(setup.replace(&path, b"read earlier", b"ours"), Err(BridgeError::Changed(_))));
        assert_eq!(fs::read(&path).unwrap(), b"theirs");
        assert!(!directory.path().join("localconfig.vdf.openrigcue").exists());
        setup.replace(&path, b"theirs", b"ours").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"ours");
        assert_eq!(fs::read(directory.path().join("localconfigsrs_bck.vdf")).unwrap(), b"read earlier");
    }

    /// A Flatpak Steam only sees its own folder, so the bridge goes there.
    #[test]
    fn flatpak_steam_gets_the_files_in_its_own_folder() {
        let home = Path::new("/home/u");
        let setup = BridgeSetup::with_locations(home, None, Path::new("/home/u/.local/share"), Path::new("/proc"));
        assert_eq!(
            setup.deploy_dir(Path::new("/home/u/.local/share/Steam")),
            Path::new("/home/u/.local/share/openrigcue/bridge")
        );
        assert_eq!(
            setup.deploy_dir(Path::new("/home/u/.var/app/com.valvesoftware.Steam/.local/share/Steam")),
            Path::new("/home/u/.var/app/com.valvesoftware.Steam/data/openrigcue/bridge")
        );
    }

    #[test]
    fn text_that_is_not_steams_is_left_alone() {
        assert_eq!(launch_options("", "1"), Lookup::Malformed);
        assert_eq!(launch_options("\"Other\"\n{\n}\n", "1"), Lookup::Malformed);
        assert_eq!(
            launch_options("\"UserLocalConfigStore\"\n{\n\t\"a\"\t\t\"unterminated\n}\n", "1"),
            Lookup::Malformed
        );
        assert_eq!(launch_options("\"UserLocalConfigStore\"\n{\n", "1"), Lookup::Malformed);
        assert_eq!(set_launch_options("\"UserLocalConfigStore\"\n{\n", "1", "x"), None);
        // An empty store gets the whole path.
        let changed = set_launch_options("\"UserLocalConfigStore\"\n{\n}\n", "7", "r %command%").unwrap();
        assert_eq!(
            changed,
            "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"7\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"r %command%\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n"
        );
    }
}

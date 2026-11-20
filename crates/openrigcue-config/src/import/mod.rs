//! Importing an existing Sim Racing Studio setup, and building the first-run setup.

pub mod configobj;
pub mod de;
pub mod json;
pub mod migrate;
pub mod pickle;
mod reconcile;
pub mod registry;
pub(crate) use reconcile::unchanged as unchanged_projection;
pub use reconcile::{reconcile, reconcile_detached};
pub mod shipped;
pub mod sqlitedict;
pub mod value;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::profiles::ProfileStore;
use crate::settings::{AxisMapping, Configuration, Dismissals, JoystickProfiles, Settings};
use crate::tunes::{FactoryTunes, ShakerLayouts, SpeakerLayout, Tune};
use de::from_legacy;
use migrate::{LegacyLayouts, LegacyState, MigrationError, MigrationReport, Platform};
use value::{Dict, Key, LegacyValue};

/// Something an import (or a conversion) could not carry over exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportWarning {
    /// Where: `SECTION.key`, `profile <game>/<vehicle>`, a file name …
    pub location: String,
    pub message: String,
}

impl ImportWarning {
    pub fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        ImportWarning { location: location.into(), message: message.into() }
    }

    /// A value OpenRigCue deliberately does not carry over (SRS sales gating, hidden-page switches,
    /// cloud, analytics, migration bookkeeping); the import accepts and ignores it.
    pub fn not_ported(location: impl Into<String>) -> Self {
        ImportWarning::new(
            location,
            "not ported (sales gating, hidden page, cloud, analytics or migration bookkeeping); ignored",
        )
    }

    /// A key present in SRS files that nothing reads (the template's `APP.window_with`, a
    /// misspelling of `window_width`); it has no effect, so the import ignores it.
    pub fn unread(location: impl Into<String>) -> Self {
        ImportWarning::new(location, "read by no SRS code; ignored")
    }

    /// Whether this warning only notes a key that is deliberately ignored ([`Self::not_ported`],
    /// [`Self::unread`]) rather than a value that failed to carry over.
    pub fn is_ignored_key(&self) -> bool {
        self.message.starts_with("not ported") || self.message.starts_with("read by no SRS code")
    }
}

impl std::fmt::Display for ImportWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// Why an SRS folder could not be imported.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{0} not found")]
    MissingFile(PathBuf),
    #[error("cannot read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error(transparent)]
    Ini(#[from] configobj::IniError),
    #[error(transparent)]
    Db(#[from] sqlitedict::DbError),
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error("{path}: {source}")]
    Json { path: PathBuf, source: json::JsonError },
}

/// A converted SRS setup.
#[derive(Debug, Clone)]
pub struct SrsImport {
    pub configuration: Configuration,
    /// Values that were not carried over exactly.
    pub warnings: Vec<ImportWarning>,
    /// The migration steps that ran to bring the files to the current schema.
    pub report: MigrationReport,
}

/// Imports an SRS user folder (`Documents\SimRacingStudio 2.0`, or a copy/backup of it): reads
/// `config.ini`, `profile.db` and `metadata.db` (and a leftover `profiles.json`), runs the
/// migration steps the files have not seen yet, with the branches for
/// `platform`, and converts the result. Nothing in the folder is modified.
///
/// In a folder with the legacy single-database layout,
/// a missing `profile.db` or `metadata.db` is taken from `config.db`.
pub fn import_srs_folder(dir: &Path, platform: Platform) -> Result<SrsImport, ImportError> {
    let config_path = dir.join("config.ini");
    if !config_path.is_file() {
        return Err(ImportError::MissingFile(config_path));
    }
    let legacy_db = dir.join("config.db");
    let profile_path = dir.join("profile.db");
    let profile_source = database_or_legacy(&profile_path, &legacy_db).ok_or(ImportError::MissingFile(profile_path))?;
    let metadata_source = database_or_legacy(&dir.join("metadata.db"), &legacy_db);
    let config_text = std::fs::read_to_string(&config_path)
        .map_err(|source| ImportError::Io { path: config_path.clone(), source })?;
    let mut state = LegacyState {
        config: configobj::parse(&config_text)?,
        profiles: sqlitedict::read_table(&profile_source, "unnamed")?,
        ..LegacyState::default()
    };
    if let Some(metadata) = &metadata_source {
        state.shaker_layouts = sqlitedict::read_table(metadata, "shakers")?;
        state.joysticks = sqlitedict::read_table(metadata, "joystick")?;
        state.game_config_notifications = sqlitedict::read_table(metadata, "game_config_notifications")?;
        state.notifications = sqlitedict::read_table(metadata, "notifications")?;
    }
    let legacy_layouts = read_legacy_layouts(&dir.join("profiles.json"))?;
    let data = shipped::ShippedData::get();
    let report = migrate::migrate(&mut state, &data.sources(platform, &legacy_layouts))?;
    let (mut configuration, warnings) = convert_state(&state);
    configuration.settings.remember_ini_layout(&config_text);
    Ok(SrsImport { configuration, warnings, report })
}

/// Which file holds a database:
/// the database file itself, else the legacy `config.db`, else nothing.
fn database_or_legacy(database: &Path, legacy_db: &Path) -> Option<PathBuf> {
    [database, legacy_db].into_iter().find(|p| p.is_file()).map(Path::to_path_buf)
}

/// `<docs>/profiles.json` as step v99 reads it. A file that does not open, is not UTF-8 or
/// is not valid JSON is [`LegacyLayouts::Unreadable`] (the step falls back to
/// the shipped file); valid JSON that openrigcue cannot hold fails the import.
pub(crate) fn read_legacy_layouts(path: &Path) -> Result<LegacyLayouts, ImportError> {
    if path.symlink_metadata().is_err() {
        return Ok(LegacyLayouts::Absent);
    }
    let Some(text) = std::fs::read(path).ok().and_then(|bytes| String::from_utf8(bytes).ok()) else {
        return Ok(LegacyLayouts::Unreadable);
    };
    match json::parse(&text) {
        Ok(LegacyValue::Dict(layouts)) => Ok(LegacyLayouts::Layouts(layouts)),
        Ok(other) => Ok(LegacyLayouts::NotAnObject(other.type_name())),
        // Decoding, number and recursion failures are handled alike:
        // the shipped layout file is read instead.
        Err(_) => Ok(LegacyLayouts::Unreadable),
    }
}

/// Converts migrated stores into the typed configuration. Entries that do not convert are
/// reported: a game tune falls back to its factory tune (or is skipped without one), a vehicle
/// tune, layout or joystick profile is skipped.
pub fn convert_state(state: &LegacyState) -> (Configuration, Vec<ImportWarning>) {
    let (mut settings, mut warnings) = Settings::from_legacy(&state.config);
    // The App page reads the launcher flags as yes/no text; unset (no Windows
    // steps ran, or an imported folder without its registry) keeps the defaults.
    for (name, field) in [
        ("StartWithWindows", &mut settings.app.start_with_login),
        ("ShowSplashScreen", &mut settings.app.show_splash_screen),
    ] {
        if let Some(value) = state.registry.get(name).and_then(LegacyValue::as_str).and_then(value::str2bool) {
            *field = value;
        }
    }
    let profiles = convert_profiles(&state.profiles, &mut warnings);
    let shaker_layouts =
        ShakerLayouts(convert_entries::<SpeakerLayout>(&state.shaker_layouts, "shaker layout", &mut warnings));
    let joysticks = JoystickProfiles(convert_entries::<BTreeMap<String, AxisMapping>>(
        &state.joysticks,
        "joystick profile",
        &mut warnings,
    ));
    let dismissals = Dismissals {
        game_setup: state.game_config_notifications.str_keys().into_iter().collect(),
        notification_keys: state.notifications.str_keys().into_iter().collect(),
    };
    let mut configuration =
        Configuration { settings, profiles, shaker_layouts, joysticks, dismissals, metadata: Default::default() };
    configuration.retain_metadata(state);
    (configuration, warnings)
}

fn convert_entries<T: serde::de::DeserializeOwned>(
    table: &Dict,
    what: &str,
    warnings: &mut Vec<ImportWarning>,
) -> BTreeMap<String, T> {
    let mut out = BTreeMap::new();
    for (key, value) in table.iter() {
        match from_legacy::<T>(value) {
            Ok(v) => {
                out.insert(key.to_text(), v);
            }
            Err(e) => warnings.push(ImportWarning::new(format!("{what} {:?}", key.to_text()), format!("{e}; skipped"))),
        }
    }
    out
}

pub(crate) fn convert_profiles(table: &Dict, warnings: &mut Vec<ImportWarning>) -> ProfileStore {
    let factory = FactoryTunes::builtin();
    let mut store = ProfileStore::default();
    for (game, value) in table.iter() {
        let game = game.to_text();
        let location = format!("profile {game:?}");
        let value = value.resolved();
        let Some(dict) = value.as_dict() else {
            warnings.push(ImportWarning::new(location, format!("is a {}, not a dict; skipped", value.type_name())));
            continue;
        };
        let mut game_dict = dict.clone();
        let vehicles = game_dict.remove("vehicles");
        game_dict.insert("vehicles", LegacyValue::Dict(Dict::new()));
        let mut tune = match Tune::from_legacy(&LegacyValue::Dict(game_dict)) {
            Ok(t) => t,
            Err(e) => match factory.get(&game) {
                Some(f) => {
                    warnings.push(ImportWarning::new(&location, format!("{e}; factory tune used")));
                    let mut f = f.clone();
                    f.vehicles.clear();
                    f
                }
                None => {
                    warnings.push(ImportWarning::new(&location, format!("{e}; skipped")));
                    continue;
                }
            },
        };
        if let Some(LegacyValue::Dict(vehicles)) = vehicles.map(|value| value.resolved()) {
            for (vehicle, v) in vehicles.iter() {
                match Tune::from_legacy(v) {
                    Ok(vt) => {
                        tune.vehicles.insert(vehicle.to_text(), vt);
                    }
                    Err(e) => warnings.push(ImportWarning::new(
                        format!("profile {game:?} vehicle {:?}", vehicle.to_text()),
                        format!("{e}; skipped"),
                    )),
                }
            }
        }
        store.insert(&game, tune);
    }
    store.retain_original(table.clone());
    store
}

/// The typed stores back as the values the migration chain reads from `profile.db` and
/// `metadata.db` (file key names; the inverse of [`convert_state`] for those stores), for a start
/// that runs the chain over stored files (`config.ini` missing, the databases present).
pub(crate) fn legacy_profiles(store: &ProfileStore) -> Dict {
    store.native_table()
}

/// A `metadata.db` table of serializable entries (`shakers`, `joystick`) as raw values.
pub(crate) fn legacy_table<T: serde::Serialize>(entries: &BTreeMap<String, T>) -> Dict {
    entries
        .iter()
        .map(|(name, entry)| {
            let json = serde_json::to_value(entry).expect("a stored entry always serializes to JSON");
            (Key::from(name.as_str()), LegacyValue::from(json))
        })
        .collect()
}

/// A `metadata.db` notification table: every writer stores `True` under the key.
pub(crate) fn legacy_key_set<'a>(keys: impl IntoIterator<Item = &'a String>) -> Dict {
    keys.into_iter().map(|key| (Key::from(key.as_str()), LegacyValue::Bool(true))).collect()
}

/// The template `profile.db` as it is used when `config.ini` already exists:
/// no migration step runs on a version-412 `config.ini`, so the template tunes are kept
/// unchanged. Tunes that do not convert are handled as on import.
pub(crate) fn template_profile_store() -> (ProfileStore, Vec<ImportWarning>) {
    let mut warnings = Vec::new();
    let store = convert_profiles(&shipped::ShippedData::get().template_profiles, &mut warnings);
    (store, warnings)
}

/// The configuration of a brand-new installation on the Windows platform (the
/// default); see [`first_run_for`].
pub fn first_run() -> &'static Configuration {
    first_run_for(Platform::Windows)
}

/// The configuration of a brand-new installation on `platform`, built once per platform:
/// the templates (template configuration, template `profile.db`, shaker layouts) run
/// through every migration step with the first-installation branches. On Windows the steps that
/// read installer registry values (v230 `Soundcard`/`MotionModel`) find none,
/// so the template values stay; on Linux the magicbox branches apply.
pub fn first_run_for(platform: Platform) -> &'static Configuration {
    static WINDOWS: OnceLock<Configuration> = OnceLock::new();
    static LINUX: OnceLock<Configuration> = OnceLock::new();
    let cell = match platform {
        Platform::Windows => &WINDOWS,
        Platform::Linux => &LINUX,
    };
    cell.get_or_init(|| {
        let data = shipped::ShippedData::get();
        let mut state = data.fresh_install_state();
        let no_legacy_layouts = LegacyLayouts::Absent;
        migrate::migrate(&mut state, &data.sources(platform, &no_legacy_layouts))
            .expect("the shipped templates migrate cleanly");
        let (configuration, warnings) = convert_state(&state);
        debug_assert!(warnings.iter().all(ImportWarning::is_ignored_key), "{warnings:?}");
        configuration
    })
}

#[cfg(test)]
mod tests;

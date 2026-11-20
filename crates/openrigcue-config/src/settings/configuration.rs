//! The whole openrigcue configuration and where it lives on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::paths::{read_toml, ConfigDir};
use super::{Dismissals, JoystickProfiles, Settings};
use crate::import::migrate::{
    self, LegacyState, MigrationError, MigrationSources, Platform, RuntimeDatabases, StartEffects,
};
use crate::import::shipped::ShippedData;
use crate::import::value::Dict;
use crate::import::{convert_state, legacy_key_set, legacy_profiles, legacy_table, sqlitedict, template_profile_store};
use crate::patch::PatchContext;
use crate::profiles::ProfileStore;
use crate::tunes::ShakerLayouts;
use crate::StoreError;

/// Title of the message box shown when a start with the stored files failed and the
/// configuration files were reset.
pub const RESET_NOTICE_TITLE: &str = "Ops, something is not right with the config files...";
/// Text of that message box.
pub const RESET_NOTICE_TEXT: &str =
    "There is an issue with the app config files, the app will restart and reset to the default settings.";

/// Everything kept in `config.ini`, `profile.db` and `metadata.db`.
#[derive(Debug, Clone)]
pub struct Configuration {
    pub settings: Settings,
    pub profiles: ProfileStore,
    pub shaker_layouts: ShakerLayouts,
    pub joysticks: JoystickProfiles,
    pub dismissals: Dismissals,
    pub(crate) metadata: super::metadata::Metadata,
}

// Equality of consumer views. Raw metadata is compared explicitly through `metadata_table`.
impl PartialEq for Configuration {
    fn eq(&self, other: &Self) -> bool {
        self.settings == other.settings
            && self.profiles == other.profiles
            && self.shaker_layouts == other.shaker_layouts
            && self.joysticks == other.joysticks
            && self.dismissals == other.dismissals
    }
}

/// The configuration a start found or created ([`Configuration::open_for`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub configuration: Configuration,
    /// This start ran from the template `config.ini` (`first_time_installation` was `yes`).
    pub first_installation: bool,
    /// Windows platform only: a start with the stored files failed, so
    /// `reset.txt` was written, the files were deleted and the start ran again from the templates.
    /// Holds why the last failed attempt failed (the text written to `reset.txt`); the UI shows
    /// [`RESET_NOTICE_TITLE`] / [`RESET_NOTICE_TEXT`].
    pub reset: Option<String>,
}

/// `<docs>/reset.txt`: written when a start failed on the Windows platform; its
/// presence makes the next start delete the configuration files.
pub const RESET_FILE: &str = "reset.txt";

/// Why a start failed.
#[derive(Debug, thiserror::Error)]
pub enum StartFailure {
    /// A stored file could not be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A migration step failed.
    #[error(transparent)]
    Migration(#[from] MigrationError),
    /// The speaker layout named by `SHAKEBOX.profile` is not in the shakers table.
    #[error("{} Error message: {}", corrupted(config_file), py_str_repr(layout))]
    MissingSpeakerLayout { config_file: PathBuf, layout: String },
    /// `profile.db` has no `emulator` (stand-by) tune.
    #[error("{} Error message: 'emulator'", corrupted(config_file))]
    MissingEmulatorTune { config_file: PathBuf },
}

/// The leading text of the corrupted-file messages, before the detail.
fn corrupted(config_file: &Path) -> String {
    format!(
        "{} file is corrupted. Please delete it and restart OpenRigCue. More info: check {} \
         or open OpenRigCue Help from the application header.",
        config_file.display(),
        config_file.parent().unwrap_or_else(|| Path::new(".")).join("log.txt").display()
    )
}

/// The quoted, escaped form of a layout name used in the message.
fn py_str_repr(text: &str) -> String {
    openrigcue_model::python_repr::repr_str(text)
}

impl Configuration {
    /// The configuration of a brand-new installation on the default platform, Windows
    /// (see [`crate::import::first_run`]).
    pub fn first_run() -> Configuration {
        crate::import::first_run().clone()
    }

    /// The configuration of a brand-new installation on `platform` (`--original-platform`).
    pub fn first_run_for(platform: Platform) -> Configuration {
        crate::import::first_run_for(platform).clone()
    }

    /// Loads the stored files (or the older TOML store); absent settings use Windows first-run values.
    pub fn load(dir: &ConfigDir) -> Result<Configuration, StoreError> {
        Configuration::load_for(dir, Platform::Windows)
    }

    /// Reads the stored files without changing anything; with no stored settings in either layout, the
    /// first-run configuration of `platform` (`--original-platform`). This is a plain read: the
    /// start of the app is [`Configuration::open_for`], which fills missing files and runs the
    /// start checks.
    pub fn load_for(dir: &ConfigDir, platform: Platform) -> Result<Configuration, StoreError> {
        let Some(settings) = load_settings(dir)? else {
            return Ok(Configuration::first_run_for(platform));
        };
        let mut state = load_metadata(dir)?.unwrap_or_default();
        state.profiles = load_profiles(dir)?.unwrap_or_default().native_table();
        let (mut configuration, _) = convert_state(&state);
        configuration.settings = settings;
        Ok(configuration)
    }

    /// [`Configuration::open_with`] without a game-patch context: step v214 (re-patching DCS) is
    /// not performed.
    pub fn open_for(dir: &ConfigDir, platform: Platform) -> Result<Opened, StartFailure> {
        Configuration::open_with(dir, platform, None)
    }

    /// The configuration part of a start:
    ///
    /// 1. Each missing file (`profile.db`, `metadata.db`, `config.ini`) is filled from its template
    ///    on its own. The template `metadata.db` has no `shakers`, `joystick` or notification table,
    ///    so absent tables start empty.
    /// 2. Migration: with `config.ini` missing, the template (`config_file_version` 84,
    ///    `first_time_installation = yes`) runs every step over the stored or template databases,
    ///    reading `<docs>/profiles.json` at v99 and performing the steps' file side effects
    ///    (`patch` is the game-patch context of v214). Stored older INI files run their remaining steps;
    ///    current files run none. `config.ini` is written on every start.
    /// 3. Checks: the selected speaker layout must be in the shakers table and the
    ///    `emulator` tune in `profile.db`, or the start fails.
    ///
    /// A failed start ends differently per platform. On Windows, `reset.txt` is written with the
    /// failure text, [`RESET_NOTICE_TEXT`] is shown, the files are deleted because `reset.txt`
    /// exists, and the start runs again. This repeats for as long as the start fails; the result
    /// is a first installation with [`Opened::reset`] set. A `reset.txt` that cannot be written ends
    /// the start with that error. On Linux the failure is returned.
    pub fn open_with(
        dir: &ConfigDir,
        platform: Platform,
        patch: Option<&PatchContext<'_>>,
    ) -> Result<Opened, StartFailure> {
        Self::open_with_profile_loading(dir, platform, patch, false).map(|(opened, _)| opened)
    }

    /// Runtime migrations and startup use canonical row/iteration boundaries.
    /// Database-backed callers must keep reading other rows canonically;
    /// use load/open_with when an explicit complete snapshot is required.
    #[doc(hidden)]
    pub fn open_for_runtime(
        dir: &ConfigDir,
        platform: Platform,
        patch: Option<&PatchContext<'_>>,
    ) -> Result<Opened, StartFailure> {
        Self::open_runtime_session(dir, platform, patch).map(|(opened, _)| opened)
    }

    /// Runtime startup plus ownership of the dictionaries opened by its
    /// constructors and migrations. The service retains these exact workers.
    #[doc(hidden)]
    pub fn open_runtime_session(
        dir: &ConfigDir,
        platform: Platform,
        patch: Option<&PatchContext<'_>>,
    ) -> Result<(Opened, RuntimeDatabases), StartFailure> {
        Self::open_with_profile_loading(dir, platform, patch, true)
    }

    fn open_with_profile_loading(
        dir: &ConfigDir,
        platform: Platform,
        patch: Option<&PatchContext<'_>>,
        runtime_profiles: bool,
    ) -> Result<(Opened, RuntimeDatabases), StartFailure> {
        preserve_typed_integer_recovery_input(dir)?;
        if platform != Platform::Windows {
            return Configuration::start(dir, platform, patch, runtime_profiles);
        }
        // Preserve the older-layout registry table before any reset deletes metadata.
        // Inaccessible registry storage reads as absent.
        let _ = crate::import::registry::migrate_legacy(dir.root());
        let mut reset = None;
        loop {
            reset_config_files(dir);
            match Configuration::start(dir, platform, patch, runtime_profiles) {
                Ok((mut opened, databases)) => {
                    opened.reset = reset;
                    return Ok((opened, databases));
                }
                Err(failure) => {
                    let text = failure.to_string();
                    let path = dir.root().join(RESET_FILE);
                    std::fs::write(&path, &text).map_err(|e| StoreError::io(&path, e))?;
                    reset = Some(text);
                }
            }
        }
    }

    /// One run of steps 1-3 of [`Configuration::open_with`].
    fn start(
        dir: &ConfigDir,
        platform: Platform,
        patch: Option<&PatchContext<'_>>,
        runtime_profiles: bool,
    ) -> Result<(Opened, RuntimeDatabases), StartFailure> {
        std::fs::create_dir_all(dir.root()).map_err(|error| StoreError::io(dir.root(), error))?;
        let legacy = dir.root().join("config.db");
        if legacy.is_file() {
            for path in [dir.profiles_file(), dir.metadata_file()] {
                if !path.is_file() {
                    std::fs::copy(&legacy, &path).map_err(|error| StoreError::io(&path, error))?;
                }
            }
        }
        let mut stored_settings = load_settings(dir)?;
        if let Some(settings) = stored_settings.as_mut() {
            settings.recover_legacy_typed_integers(&dir.settings_file(), &dir.old_settings_file());
        }
        let profile_file = dir.profiles_file();
        let current_runtime = runtime_profiles
            && stored_settings.as_ref().is_some_and(|settings| {
                settings
                    .native_snapshot()
                    .get("APP")
                    .and_then(crate::import::value::LegacyValue::as_dict)
                    .and_then(|app| app.get("config_file_version"))
                    .and_then(crate::import::value::LegacyValue::py_int)
                    .is_some_and(|version| version >= i64::from(migrate::CURRENT_VERSION))
            });
        let selected_profile = runtime_profiles && profile_file.is_file();
        let stored_profiles = if selected_profile {
            // Migration helpers read rows at their own points. The final
            // check loads the emulator tune after the INI write.
            Some(ProfileStore::default())
        } else {
            load_profiles(dir)?
        };
        // The start checks never iterate joystick/notification values. Production
        // callers use the retained dictionaries for selected reads/keys.
        let stored_metadata = load_metadata_for_start(dir, runtime_profiles && dir.metadata_file().is_file())?;
        let first_installation = stored_settings.as_ref().is_none_or(|settings| {
            settings
                .native_snapshot()
                .get("APP")
                .and_then(crate::import::value::LegacyValue::as_dict)
                .and_then(|app| app.get("first_time_installation"))
                .and_then(crate::import::value::LegacyValue::as_str)
                == Some("yes")
        });

        let mut databases = RuntimeDatabases::default();
        let mut configuration = match stored_settings {
            None => {
                let data = ShippedData::get();
                let mut state = data.fresh_install_state();
                if let Some(profiles) = &stored_profiles {
                    state.profiles = legacy_profiles(profiles);
                }
                if let Some(metadata) = stored_metadata {
                    state.shaker_layouts = metadata.shaker_layouts;
                    state.joysticks = metadata.joysticks;
                    state.game_config_notifications = metadata.game_config_notifications;
                    state.notifications = metadata.notifications;
                }
                seed_start_files(dir, &state, true)?;
                let legacy_layouts = migrate::LegacyLayouts::DeferredFile(dir.root().join("profiles.json"));
                let sources = MigrationSources {
                    effects: Some(StartEffects { docs: dir.root(), patch }),
                    ..data.sources(platform, &legacy_layouts)
                };
                if runtime_profiles {
                    open_initial_databases(dir, &mut databases)?;
                    migrate::migrate_for_runtime(&mut state, &sources, &mut databases)?;
                } else {
                    migrate::migrate(&mut state, &sources)?;
                }
                let (configuration, _not_carried_over) = convert_state(&state);
                if runtime_profiles {
                    configuration.settings.write_ini_only(&dir.settings_file())?;
                    open_final_databases(dir, &mut databases)?;
                } else {
                    configuration.save(dir)?;
                }
                configuration
            }
            Some(settings) => {
                let native_ini = dir.settings_file().is_file();
                let profiles = match stored_profiles {
                    Some(profiles) => profiles,
                    None => {
                        let (profiles, _not_carried_over) = template_profile_store();
                        profiles.save_all(&profile_file)?;
                        profiles
                    }
                };
                // Convert an older TOML folder on startup. Existing TOML files remain as a backup.
                if !profile_file.exists() {
                    profiles.save_all(&profile_file)?;
                }
                let mut state = stored_metadata.unwrap_or_default();
                state.config = settings.native_snapshot();
                state.profiles = profiles.native_table();
                if runtime_profiles {
                    seed_start_files(dir, &state, false)?;
                }
                let before = state.clone();
                let data = ShippedData::get();
                let layouts = migrate::LegacyLayouts::DeferredFile(dir.root().join("profiles.json"));
                let sources = MigrationSources {
                    effects: Some(StartEffects { docs: dir.root(), patch }),
                    ..data.sources(platform, &layouts)
                };
                if runtime_profiles {
                    open_initial_databases(dir, &mut databases)?;
                    migrate::migrate_for_runtime(&mut state, &sources, &mut databases)?;
                } else {
                    migrate::migrate(&mut state, &sources)?;
                }
                let (mut configuration, _) = convert_state(&state);
                configuration.settings.retain_ini_layout(&settings);
                // The registry-backed switches have Linux equivalents outside config.ini.
                if !state.registry.contains("StartWithWindows") {
                    configuration.settings.app.start_with_login = settings.app.start_with_login;
                }
                if !state.registry.contains("ShowSplashScreen") {
                    configuration.settings.app.show_splash_screen = settings.app.show_splash_screen;
                }
                if !runtime_profiles && state.profiles != before.profiles {
                    configuration.profiles.save_all(&profile_file)?;
                }
                if !runtime_profiles
                    && (!dir.metadata_file().exists()
                        || state.shaker_layouts != before.shaker_layouts
                        || state.joysticks != before.joysticks
                        || state.game_config_notifications != before.game_config_notifications
                        || state.notifications != before.notifications)
                {
                    configuration.save_metadata(dir)?;
                } else if !runtime_profiles {
                    for name in super::metadata::TABLES {
                        sqlitedict::ensure_table(&dir.metadata_file(), name)
                            .map_err(|error| StoreError::parse(&dir.metadata_file(), error))?;
                    }
                }
                // Migration ends with an INI write on every start.
                if runtime_profiles && native_ini {
                    let write = configuration.settings.write_ini_only(&dir.settings_file());
                    if current_runtime {
                        if let Err(error) = write {
                            tracing::error!(%error, "error while saving config file in the config_helpers");
                        }
                    } else {
                        write?;
                    }
                } else {
                    configuration.settings.save(&dir.settings_file())?;
                }
                if runtime_profiles {
                    open_final_databases(dir, &mut databases)?;
                }
                configuration
            }
        };

        let layout = if runtime_profiles {
            let raw = configuration.settings.native_snapshot();
            let section =
                raw.get("SHAKEBOX").ok_or_else(|| StoreError::parse(&dir.settings_file(), "KeyError: 'SHAKEBOX'"))?;
            let value = section.item("profile").map_err(|error| {
                StoreError::parse(
                    &dir.settings_file(),
                    format!("{}: {error}", if section.as_dict().is_some() { "KeyError" } else { "TypeError" }),
                )
            })?;
            value
                .as_str()
                .ok_or_else(|| {
                    StoreError::parse(
                        &dir.settings_file(),
                        format!("TypeError: unsupported profile key {}", value.type_name()),
                    )
                })?
                .to_owned()
        } else {
            configuration.settings.shakers.speaker_layout.clone()
        };
        if runtime_profiles {
            // Any successfully decoded selected object is accepted,
            // even null. Unrelated values are not decoded here.
            let row = databases
                .metadata
                .get("shakers")
                .expect("startup shaker dictionary")
                .get(&layout)
                .map_err(|error| StoreError::parse(&dir.metadata_file(), error))?
                .ok_or_else(|| StartFailure::MissingSpeakerLayout {
                    config_file: dir.settings_file(),
                    layout: layout.clone(),
                })?;
            configuration
                .replace_native_shakers([(crate::import::value::Key::from(layout), row)].into_iter().collect());
        } else if !configuration.shaker_layouts.0.contains_key(&layout) {
            return Err(StartFailure::MissingSpeakerLayout { config_file: dir.settings_file(), layout });
        }
        if runtime_profiles {
            let row = databases
                .profile
                .as_ref()
                .expect("startup profile dictionary")
                .get("emulator")
                .map_err(|error| StoreError::parse(&profile_file, error))?;
            configuration.profiles.refresh_native_game("emulator", row);
        }
        if configuration.profiles.get("emulator").is_none() {
            return Err(StartFailure::MissingEmulatorTune { config_file: dir.settings_file() });
        }
        Ok((Opened { configuration, first_installation, reset: None }, databases))
    }

    /// Writes every file: `profile.db`, then `metadata.db`, then `config.ini`, the order in which
    /// a start creates them.
    pub fn save(&self, dir: &ConfigDir) -> Result<(), StoreError> {
        self.profiles.save_all(&dir.profiles_file())?;
        self.save_metadata(dir)?;
        self.settings.save(&dir.settings_file())
    }

    /// Raw metadata with only actual typed-view edits applied.
    pub fn metadata_table(&self, name: &str) -> Dict {
        self.metadata.table(self, name)
    }

    /// Accepts the canonical joystick table after a database assignment/deletion. Entries which cannot
    /// populate the typed view remain in the table for the UI and mapping thread to read verbatim.
    pub fn replace_native_joysticks(&mut self, table: Dict) {
        self.joysticks.0 = table
            .iter()
            .filter_map(|(key, value)| crate::import::de::from_legacy(value).ok().map(|entry| (key.to_text(), entry)))
            .collect();
        let mut metadata = self.metadata.clone();
        metadata.accept(self, "joystick", table);
        self.metadata = metadata;
    }

    /// Accepts a canonical shaker row write, retaining fields outside the
    /// typed speaker projection and the raw rows it cannot represent.
    pub fn replace_native_shakers(&mut self, table: Dict) {
        self.shaker_layouts.0 = table
            .iter()
            .filter_map(|(key, value)| crate::import::de::from_legacy(value).ok().map(|entry| (key.to_text(), entry)))
            .collect();
        let mut metadata = self.metadata.clone();
        metadata.accept(self, "shakers", table);
        self.metadata = metadata;
    }

    /// Accept a canonical notification-table write without losing raw values
    /// of the other cached entries. Their consumer view is key membership.
    pub fn replace_native_notifications(&mut self, name: &str, table: Dict) {
        let keys = table.str_keys().into_iter().collect();
        match name {
            "notifications" => self.dismissals.notification_keys = keys,
            "game_config_notifications" => self.dismissals.game_setup = keys,
            _ => unreachable!("notification metadata table"),
        }
        let mut metadata = self.metadata.clone();
        metadata.accept(self, name, table);
        self.metadata = metadata;
    }

    pub(crate) fn retain_metadata(&mut self, state: &LegacyState) {
        let mut metadata = super::metadata::Metadata::default();
        metadata.retain(state, self);
        self.metadata = metadata;
    }

    pub fn save_metadata(&self, dir: &ConfigDir) -> Result<(), StoreError> {
        std::fs::create_dir_all(dir.root()).map_err(|error| StoreError::io(dir.root(), error))?;
        for name in super::metadata::TABLES {
            sqlitedict::replace_table(&dir.metadata_file(), name, &self.metadata_table(name))
                .map_err(|error| StoreError::parse(&dir.metadata_file(), error))?;
        }
        Ok(())
    }

    /// Applies autocommit changes to one metadata table and acknowledges its resulting row order.
    pub fn commit_metadata(&mut self, dir: &ConfigDir, name: &str, before: &Configuration) -> Result<(), StoreError> {
        let committed = super::metadata::commit_changes(
            &dir.metadata_file(),
            name,
            &before.metadata_table(name),
            &self.metadata_table(name),
        )?;
        let mut metadata = self.metadata.clone();
        metadata.accept(self, name, committed);
        self.metadata = metadata;
        Ok(())
    }
}

/// First-install template copies precede migrations. Keep existing files,
/// and seed raw imported rows when upgrading an older TOML-only folder.
fn seed_start_files(dir: &ConfigDir, state: &LegacyState, copy_ini: bool) -> Result<(), StoreError> {
    let profile = dir.profiles_file();
    if !profile.exists() {
        sqlitedict::replace_table(&profile, "unnamed", &state.profiles)
            .map_err(|error| StoreError::parse(&profile, error))?;
    }
    let metadata = dir.metadata_file();
    let new_metadata = !metadata.exists();
    if new_metadata {
        // The template profile.db also seeds metadata.db, including
        // the unnamed template table, before the metadata tables are opened.
        sqlitedict::replace_table(&metadata, "unnamed", &ShippedData::get().template_profiles)
            .map_err(|error| StoreError::parse(&metadata, error))?;
    }
    let ini = dir.settings_file();
    if copy_ini && !ini.exists() {
        super::paths::write_in_place(&ini, include_bytes!("../../data/srs/template.ini"))?;
    }
    // The metadata tables are opened before the migration steps run.
    // There is no push delivery or cloud tuner. Notifications
    // and cache open at the end of migration, after a successful chain.
    for (table, rows) in [
        ("joystick", &state.joysticks),
        ("shakers", &state.shaker_layouts),
        ("game_config_notifications", &state.game_config_notifications),
    ] {
        if new_metadata {
            sqlitedict::replace_table(&metadata, table, rows)
        } else {
            sqlitedict::ensure_table(&metadata, table)
        }
        .map_err(|error| StoreError::parse(&metadata, error))?;
    }
    if new_metadata && !state.notifications.is_empty() {
        sqlitedict::replace_table(&metadata, "notifications", &state.notifications)
            .map_err(|error| StoreError::parse(&metadata, error))?;
    }
    Ok(())
}

fn open_initial_databases(dir: &ConfigDir, databases: &mut RuntimeDatabases) -> Result<(), StoreError> {
    let path = dir.profiles_file();
    databases.profile = Some(
        sqlitedict::PendingTable::open(&path, "unnamed")
            .map_err(|error| StoreError::database(&path, "execute", error))?,
    );
    let path = dir.metadata_file();
    for table in ["joystick", "shakers", "game_config_notifications"] {
        databases.metadata.insert(
            table.into(),
            sqlitedict::AutocommitTable::open(&path, table)
                .map_err(|error| StoreError::database(&path, "execute", error))?,
        );
    }
    Ok(())
}

fn open_final_databases(dir: &ConfigDir, databases: &mut RuntimeDatabases) -> Result<(), StoreError> {
    let path = dir.metadata_file();
    for table in ["notifications", "cache"] {
        if !databases.metadata.contains_key(table) {
            databases.metadata.insert(
                table.into(),
                sqlitedict::AutocommitTable::open(&path, table)
                    .map_err(|error| StoreError::database(&path, "execute", error))?,
            );
        }
    }
    Ok(())
}

/// The stored table, or `template` written to `path` when the file is missing.
fn load_metadata(dir: &ConfigDir) -> Result<Option<LegacyState>, StoreError> {
    load_metadata_for_start(dir, false)
}

fn load_metadata_for_start(dir: &ConfigDir, defer_runtime_values: bool) -> Result<Option<LegacyState>, StoreError> {
    let path = if dir.metadata_file().exists() { dir.metadata_file() } else { dir.root().join("config.db") };
    if path.exists() {
        let read = |table| sqlitedict::read_table(&path, table).map_err(|error| StoreError::parse(&path, error));
        return Ok(Some(LegacyState {
            shaker_layouts: if defer_runtime_values { Dict::new() } else { read("shakers")? },
            joysticks: if defer_runtime_values { Dict::new() } else { read("joystick")? },
            game_config_notifications: if defer_runtime_values {
                Dict::new()
            } else {
                read("game_config_notifications")?
            },
            notifications: if defer_runtime_values { Dict::new() } else { read("notifications")? },
            ..LegacyState::default()
        }));
    }
    // Upgrade an older TOML store without deleting its files.
    let layouts = read_toml::<ShakerLayouts>(&dir.shaker_layouts_file())?;
    let joysticks = read_toml::<JoystickProfiles>(&dir.joysticks_file())?;
    let dismissals = read_toml::<Dismissals>(&dir.state_file())?;
    if layouts.is_none() && joysticks.is_none() && dismissals.is_none() {
        return Ok(None);
    }
    let layouts = layouts.unwrap_or_default();
    let joysticks = joysticks.unwrap_or_else(|| JoystickProfiles(BTreeMap::new()));
    let dismissals = dismissals.unwrap_or_default();
    Ok(Some(LegacyState {
        shaker_layouts: legacy_table(&layouts.0),
        joysticks: legacy_table(&joysticks.0),
        game_config_notifications: legacy_key_set(&dismissals.game_setup),
        notifications: legacy_key_set(&dismissals.notification_keys),
        ..LegacyState::default()
    }))
}

fn load_profiles(dir: &ConfigDir) -> Result<Option<ProfileStore>, StoreError> {
    let path = if dir.profiles_file().exists() {
        dir.profiles_file()
    } else if dir.root().join("config.db").is_file() {
        dir.root().join("config.db")
    } else {
        dir.tunes_dir()
    };
    if path.exists() {
        ProfileStore::load(&path).map(Some)
    } else {
        Ok(None)
    }
}

fn load_settings(dir: &ConfigDir) -> Result<Option<Settings>, StoreError> {
    let path = if dir.settings_file().exists() { dir.settings_file() } else { dir.old_settings_file() };
    if path.exists() {
        Settings::load(&path).map(Some)
    } else {
        Ok(None)
    }
}

// Run outside the Windows-platform reset/retry handling: failure to preserve the
// older TOML file must never trigger deletion of the user's configuration.
pub(super) fn preserve_typed_integer_recovery_input(dir: &ConfigDir) -> Result<(), StoreError> {
    use std::io::Write;
    let ini = dir.settings_file();
    if !ini.is_file() {
        return Ok(());
    }
    // A malformed/unrelated INI still belongs to the startup
    // error/reset path. This preflight owns only positively identified repairs.
    let Ok(mut settings) = Settings::load_ini_only(&ini) else { return Ok(()) };
    if !settings.recover_legacy_typed_integers(&ini, &dir.old_settings_file()) {
        return Ok(());
    }
    let bytes = std::fs::read(&ini).map_err(|error| StoreError::io(&ini, error))?;
    let path = dir.root().join("config.ini.before-typed-integer-recovery");
    let mut backup = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(backup) => backup,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_file() => return Ok(()),
        Err(error) => return Err(StoreError::io(&path, error)),
    };
    if let Err(error) = backup.write_all(&bytes).and_then(|()| backup.sync_all()) {
        drop(backup);
        let _ = std::fs::remove_file(&path);
        return Err(StoreError::io(&path, error));
    }
    Ok(())
}

/// When `<docs>/reset.txt` exists, `config.db`, `config.ini`,
/// `log.txt`, `reset.txt`, `restore.txt`, `metadata.db` and `profile.db` are deleted, each on its
/// own (a file that cannot be removed is reported and skipped). Returns whether it ran.
pub fn reset_config_files(dir: &ConfigDir) -> bool {
    if !dir.root().join(RESET_FILE).is_file() {
        return false;
    }
    let root = dir.root();
    let files = [
        root.join("config.db"),
        dir.settings_file(),
        dir.old_settings_file(),
        root.join("log.txt"),
        root.join(RESET_FILE),
        root.join("restore.txt"),
        dir.shaker_layouts_file(),
        dir.joysticks_file(),
        dir.state_file(),
        dir.profiles_file(),
        dir.metadata_file(),
    ];
    for path in files {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::info!("Cannot remove file {}: {error}", path.display());
        }
    }
    let tunes_dir = dir.tunes_dir();
    if let Err(error) = std::fs::remove_dir_all(&tunes_dir) {
        tracing::info!("Cannot remove file {}: {error}", tunes_dir.display());
    }
    true
}

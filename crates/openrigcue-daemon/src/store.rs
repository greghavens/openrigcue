//! The configuration the service runs with (`config.ini`, `profile.db` and `metadata.db`), kept in
//! memory, changed by the UI and written on Save.

mod launcher;
mod metadata;

use std::sync::{Arc, Mutex};

use openrigcue_config::import::migrate::Platform;
use openrigcue_config::settings::paths::ConfigDir;
use openrigcue_config::settings::{Configuration, Dismissals, JoystickProfiles, Opened, Settings, StartFailure};
use openrigcue_config::StoreError;
use openrigcue_model::native_value::LegacyValue;

pub type ProfileRows =
    Box<dyn Iterator<Item = Result<(String, openrigcue_config::import::value::LegacyValue), StoreError>> + Send>;
pub type MetadataKeys = Box<dyn Iterator<Item = Result<String, StoreError>> + Send>;

/// What the start of the configuration found, for the UI's first main-loop pass: whether this is a first
/// installation (the UI then opens Setup -> Hardware) and whether the stored files were reset
/// (the UI then shows a message box).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StartInfo {
    pub first_installation: bool,
    /// Why the stored files were reset (the launcher's `reset.txt`); `None` when the start used them.
    pub reset: Option<String>,
}

/// The configuration and where it is stored.
///
/// Setting changes apply in memory at once and reach disk on Save; the notification tables of
/// `metadata.db` are autocommitted, so dismissals are written as they change. `revision` counts in-memory changes
/// so the loops that cache derived settings know when to rebuild them.
#[derive(Debug)]
pub struct ConfigStore {
    dir: Option<ConfigDir>,
    config: Configuration,
    revision: u64,
    /// `--original-platform`, whose first-run configuration [`ConfigStore::persist_first_run`] writes.
    platform: Platform,
    /// Taken once by the UI ([`ConfigStore::take_start_info`]).
    start: Option<StartInfo>,
    profile_db: Option<openrigcue_config::import::sqlitedict::PendingTable>,
    profile_views_dirty: bool,
    metadata_db: Mutex<std::collections::BTreeMap<String, Arc<openrigcue_config::import::sqlitedict::AutocommitTable>>>,
    /// Startup/first persistence completed. Removing config.ini later must not
    /// turn an existing session back into a first-run snapshot writer.
    disk_initialized: Mutex<bool>,
    /// The published live settings tree, distinct from detached file/typed views.
    native_settings: Mutex<Option<(LegacyValue, LegacyValue, u64)>>,
}

impl ConfigStore {
    /// Loads the stored configuration, or the first-run
    /// configuration when there is none yet (the templates with every migration applied).
    ///
    /// A plain read that writes nothing (tests); the service starts through [`ConfigStore::open`].
    pub fn load(dir: ConfigDir) -> Result<ConfigStore, StoreError> {
        let disk_initialized = dir.settings_file().is_file();
        let config = Configuration::load(&dir)?;
        Ok(ConfigStore {
            dir: Some(dir),
            config,
            revision: 0,
            platform: Platform::Windows,
            start: None,
            profile_db: None,
            profile_views_dirty: true,
            metadata_db: Mutex::new(std::collections::BTreeMap::new()),
            disk_initialized: Mutex::new(disk_initialized),
            native_settings: Mutex::new(None),
        })
    }

    /// The start of the application (through
    /// [`Configuration::open_for`]): missing files are filled from the built-in templates and written, the
    /// template chain runs when settings are missing, and the start checks run. Under the Windows rules a
    /// failed start resets the files (the result says why); under the Linux rules the failure is returned and the
    /// service does not start.
    ///
    /// `patch` is where the migration step that re-patches DCS looks for the game; without it the step is skipped.
    pub fn open(
        dir: ConfigDir,
        platform: Platform,
        patch: Option<&openrigcue_config::patch::PatchContext<'_>>,
    ) -> Result<ConfigStore, StartFailure> {
        let (Opened { configuration, first_installation, reset }, databases) =
            Configuration::open_runtime_session(&dir, platform, patch)?;
        Ok(ConfigStore {
            dir: Some(dir),
            config: configuration,
            revision: 0,
            platform,
            start: Some(StartInfo { first_installation, reset }),
            profile_db: databases.profile,
            profile_views_dirty: true,
            metadata_db: Mutex::new(
                databases.metadata.into_iter().map(|(name, database)| (name, Arc::new(database))).collect(),
            ),
            disk_initialized: Mutex::new(true),
            native_settings: Mutex::new(None),
        })
    }

    pub(crate) fn port_cache_database(
        &self,
    ) -> Result<Arc<openrigcue_config::import::sqlitedict::AutocommitTable>, StoreError> {
        self.metadata_database("cache")?.ok_or_else(|| StoreError::Parse {
            path: self.dir.as_ref().map_or_else(|| "metadata.db".into(), ConfigDir::metadata_file),
            message: "runtime cache dictionary is unavailable".into(),
        })
    }

    /// A store that never writes (tests, dry runs).
    pub fn in_memory(config: Configuration) -> ConfigStore {
        ConfigStore {
            dir: None,
            config,
            revision: 0,
            platform: Platform::Windows,
            start: None,
            profile_db: None,
            profile_views_dirty: true,
            metadata_db: Mutex::new(std::collections::BTreeMap::new()),
            disk_initialized: Mutex::new(false),
            native_settings: Mutex::new(None),
        }
    }

    /// Whether this start created the configuration.
    pub(crate) fn first_installation(&self) -> bool {
        self.start.as_ref().is_some_and(|start| start.first_installation)
    }

    /// What the start found, once: the first UI main-loop pass acts on it (the window starts once per run).
    pub fn take_start_info(&mut self) -> StartInfo {
        self.start.take().unwrap_or_default()
    }

    pub fn config(&self) -> &Configuration {
        &self.config
    }

    /// One row of the profile table: decode only the requested row, from the current
    /// transaction when it is open. Memory-only stores retain their fixture view.
    pub fn profile_row(&self, key: &str) -> Result<Option<openrigcue_config::import::value::LegacyValue>, StoreError> {
        if let Some(table) = &self.profile_db {
            return table.get(key).map_err(|error| {
                StoreError::database(
                    &self.dir.as_ref().expect("disk table has a directory").profiles_file(),
                    "select_one",
                    error,
                )
            });
        }
        if let Some(dir) = &self.dir {
            let path = dir.profiles_file();
            if self.native_files_initialized() && path.exists() {
                return openrigcue_config::import::sqlitedict::read_item(&path, "unnamed", key)
                    .map_err(|error| StoreError::database(&path, "select_one", error));
            }
        }
        Ok(self.config.profiles.native_table().get(key).cloned())
    }

    /// A local edit starts with the selected row's current database value;
    /// reading it does not reorder the cached snapshot.
    pub fn config_with_profile_row(&self, key: &str) -> Result<Configuration, StoreError> {
        let row = self.profile_row(key)?;
        let mut changed = self.config.clone();
        changed.profiles.synchronize_native();
        changed.profiles.refresh_native_game(key, row);
        Ok(changed)
    }

    /// Every row of the profile table: keep statement order and decode each value only
    /// when requested, so a later decode failure retains earlier assignments.
    pub fn profile_rows(&self) -> Result<ProfileRows, StoreError> {
        if let Some(table) = &self.profile_db {
            let path = self.dir.as_ref().expect("disk table has a directory").profiles_file();
            return Self::database_profile_rows(path, table.rows());
        }
        if let Some(dir) = &self.dir {
            let path = dir.profiles_file();
            if self.native_files_initialized() && path.exists() {
                let rows = openrigcue_config::import::sqlitedict::read_rows(&path, "unnamed");
                return Self::database_profile_rows(path, rows);
            }
        }
        let rows: Vec<_> =
            self.config.profiles.native_table().iter().map(|(key, value)| Ok((key.to_text(), value.clone()))).collect();
        Ok(Box::new(rows.into_iter()))
    }

    fn database_profile_rows(
        path: std::path::PathBuf,
        rows: Result<openrigcue_config::import::sqlitedict::Rows, openrigcue_config::import::sqlitedict::DbError>,
    ) -> Result<ProfileRows, StoreError> {
        let rows = rows.map_err(|error| StoreError::database(&path, "select", error))?;
        Ok(Box::new(rows.map(move |row| row.map_err(|error| StoreError::database(&path, "select", error)))))
    }

    pub fn profile_manager_entries<'a>(
        &self,
        catalog: &'a openrigcue_config::catalog::Catalog,
        platform: Platform,
    ) -> Result<Vec<openrigcue_config::profiles::ManagerEntry<'a>>, openrigcue_config::profiles::ProfileError> {
        openrigcue_config::profiles::ProfileStore::manager_entries_from_lookup(catalog, platform, |key| {
            self.profile_row(key).map_err(|error| openrigcue_config::profiles::ProfileError::Native(error.to_string()))
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.config.settings
    }

    /// The live settings tree. Repeated reads return the same root
    /// and section handles; accepted setting assignments update those nodes
    /// before publishing the store revision. `Settings::native_snapshot` is
    /// intentionally a detached file/typed view and cannot supply this alias.
    pub fn native_settings_tree(&self) -> LegacyValue {
        let _operation = openrigcue_model::native_object::native_operation();
        let mut live = self.native_settings.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((value, _, revision)) = live.as_ref() {
            if *revision == self.revision {
                return value.clone();
            }
        }
        let after = LegacyValue::Dict(self.config.settings.native_snapshot());
        if let Some((value, before, revision)) = &mut *live {
            openrigcue_config::import::reconcile(value, before, &after);
            *before = after;
            *revision = self.revision;
            value.clone()
        } else {
            let value = LegacyValue::Object(openrigcue_model::native_value::NativeObject::new(after.clone()));
            *live = Some((value.clone(), after, self.revision));
            value
        }
    }

    pub fn dir(&self) -> Option<&ConfigDir> {
        self.dir.as_ref()
    }

    /// Number of in-memory changes so far.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Change the configuration in memory.
    pub fn update<R>(&mut self, change: impl FnOnce(&mut Configuration) -> R) -> R {
        let result = change(&mut self.config);
        self.revision += 1;
        let _live = self.native_settings_tree();
        self.profile_views_dirty = true;
        result
    }

    /// Replace the global settings in memory.
    pub fn replace_settings(&mut self, mut settings: Settings) {
        settings.retain_source(&self.config.settings);
        self.update(|config| config.settings = settings);
    }

    /// Explicit settings/launcher-flag snapshot save. UI callbacks use
    /// write_settings_native; neither path rewrites profile/metadata snapshots.
    pub fn save(&self) -> Result<(), StoreError> {
        match &self.dir {
            Some(dir) => {
                self.persist_first_run()?;
                self.config.settings.save(&dir.settings_file())
            }
            None => Ok(()),
        }
    }

    /// A settings write logs its own write errors. UI callers
    /// continue translating, updating flags and completing Save afterward.
    pub(crate) fn write_settings_native(&self) {
        let Some(dir) = &self.dir else { return };
        if let Err(error) =
            self.persist_first_run().and_then(|()| self.config.settings.write_ini_only(&dir.settings_file()))
        {
            tracing::error!(%error, "error while saving config file in the config_helpers");
        }
    }

    /// Changes one setting in memory and in the stored settings file only (the rest of the file stays as it
    /// is on disk): the file is read, one key is set and the file is written back. The Stand By notice's "Do
    /// not display this message again" uses it.
    pub fn write_setting(&mut self, change: impl Fn(&mut Settings)) -> Result<(), StoreError> {
        self.update(|config| change(&mut config.settings));
        let Some(dir) = &self.dir else { return Ok(()) };
        self.persist_first_run()?;
        let path = dir.settings_file();
        let mut stored = Settings::load_ini_only(&path)?;
        change(&mut stored);
        stored.write_ini_only(&path)
    }

    /// Assign through the required section of the live tree in memory, then in a fresh read of the stored file.
    pub fn write_native_setting(&mut self, section_name: &str, key: &str, value: &LegacyValue) -> Result<(), String> {
        fn assign(settings: &mut Settings, section_name: &str, key: &str, value: &LegacyValue) -> Result<(), String> {
            let root = LegacyValue::Dict(settings.native_snapshot());
            let mut section = root.item_owned(section_name).map_err(|detail| format!("KeyError: {detail}"))?;
            section.assign_item(key, value.clone()).map_err(|detail| format!("TypeError: {detail}"))?;
            settings.edit_native(|raw| {
                raw.insert(section_name, section);
            });
            Ok(())
        }
        let mut updated = self.config.settings.clone();
        assign(&mut updated, section_name, key, value)?;
        self.update(|configuration| configuration.settings = updated);
        let Some(dir) = &self.dir else { return Ok(()) };
        self.persist_first_run().map_err(|error| error.to_string())?;
        let path = dir.settings_file();
        let mut stored = Settings::load_ini_for_native_edit(&path).map_err(|error| error.to_string())?;
        assign(&mut stored, section_name, key, value)?;
        stored.write_ini_only(&path).map_err(|error| error.to_string())
    }

    /// The port change confirmation's write: the stored settings file is read again, its `PORT_FORWARDING`
    /// section is replaced by the one in memory, and the file is written back. Nothing else of memory reaches
    /// disk (`NETWORK_PORT` changes only in memory).
    pub fn write_port_forwarding(&self) -> Result<(), StoreError> {
        let Some(dir) = &self.dir else { return Ok(()) };
        self.persist_first_run()?;
        let path = dir.settings_file();
        let mut stored = Settings::load_ini_for_native_edit(&path)?;
        // The live tree is iterated, so an absent section is not inserted and a present
        // section is copied with its stored value kinds rather than the lenient typed port-forwarding view.
        if let Some(forwarding) = self.config.settings.native_snapshot().get("PORT_FORWARDING").cloned() {
            stored.edit_native(|raw| raw.insert("PORT_FORWARDING", forwarding));
        }
        stored.write_ini_only(&path)
    }

    /// Before the first single-file write to a folder that was never saved, writes the stored state whole.
    ///
    /// A folder is complete when `config.ini`,
    /// `profile.db` and `metadata.db` are all on disk, so that a later autocommit or settings write of one of
    /// them never stands alone. Nothing is written until a change, and [`Configuration::load`] takes the
    /// first-run configuration while settings are missing, ignoring any companion file. So the first
    /// single-file write first stores the tunes, speaker layouts, joystick profiles and notification tables as
    /// they are in memory (each of them is written at once on every change) and the first-run settings (the
    /// first-run `config.ini` holds the defaults until Save or a language change); `config.ini` is
    /// written last and marks the folder complete. Completion belongs to the
    /// session: later INI deletion never reseeds its profile/metadata databases.
    pub fn persist_first_run(&self) -> Result<(), StoreError> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let mut initialized = self.disk_initialized.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *initialized {
            return Ok(());
        }
        if dir.settings_file().exists() {
            *initialized = true;
            return Ok(());
        }
        let mut stored = self.config.clone();
        stored.settings = Configuration::first_run_for(self.platform).settings;
        stored.save(dir)?;
        *initialized = true;
        Ok(())
    }

    fn native_files_initialized(&self) -> bool {
        *self.disk_initialized.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            || self.dir.as_ref().is_some_and(|dir| dir.settings_file().is_file())
    }

    /// The no-restart API fixture can keep using the replacement first-run
    /// store after factory reset; a real restart constructs a new store.
    pub(crate) fn reset_disk_initialization(&mut self) {
        *self.disk_initialized.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        // The no-restart fixture explicitly begins a replacement session.
        // Normal close retains its table handles.
        self.profile_db = None;
        self.metadata_db.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
        self.profile_views_dirty = true;
    }

    /// Assigns games in order, then commits the entire open profile transaction. Earlier pending imports are
    /// included. A deferred SQL error can be delivered before enqueueing the
    /// commit or after that commit persists earlier successful assignments.
    pub fn commit_games<'a>(
        &mut self,
        games: impl IntoIterator<Item = &'a str>,
        mut changed: Configuration,
    ) -> Result<(), StoreError> {
        let values = changed.profiles.native_table();
        for game in games {
            self.stage_game_row(game, values.get(game).cloned())?;
        }
        changed.profiles = self.config.profiles.clone();
        self.config = changed;
        self.revision += 1;
        let _live = self.native_settings_tree();
        self.profile_views_dirty = false;
        self.commit_profiles()
    }

    /// Assigns (or deletes) one profile table row without commit. The producer
    /// queues the SQL, then updates its cached projection. Reads of the stored row
    /// use the worker, whose next boundary delivers any deferred failure.
    pub fn stage_game_row(
        &mut self,
        key: &str,
        value: Option<openrigcue_config::import::value::LegacyValue>,
    ) -> Result<(), StoreError> {
        if let Some(dir) = &self.dir {
            self.persist_first_run()?;
            let path = dir.profiles_file();
            if self.profile_db.is_none() {
                self.profile_db = Some(
                    openrigcue_config::import::sqlitedict::PendingTable::open(&path, "unnamed")
                        .map_err(|error| StoreError::database(&path, "execute", error))?,
                );
            }
            self.profile_db.as_ref().expect("opened above").assign(key, value.as_ref()).map_err(|error| {
                StoreError::database(&path, if value.is_some() { "execute" } else { "select_one" }, error)
            })?;
        }
        if self.profile_views_dirty {
            self.config.profiles.synchronize_native();
            self.profile_views_dirty = false;
        }
        self.config.profiles.accept_native_game(key, value);
        self.revision += 1;
        Ok(())
    }

    /// Commits the profile table: every earlier assignment on the connection reaches disk together.
    pub fn commit_profiles(&self) -> Result<(), StoreError> {
        if let Some(table) = &self.profile_db {
            table.commit().map_err(|error| {
                StoreError::database(
                    &self.dir.as_ref().expect("disk table has a directory").profiles_file(),
                    "commit",
                    error,
                )
            })?;
        }
        Ok(())
    }

    /// Closes the database before reset/exit, discarding uncommitted changes.
    pub(crate) fn close_profile_database(&mut self) {
        if self.profile_db.is_none() && self.native_files_initialized() {
            self.profile_db = self.dir.as_ref().map(|dir| {
                openrigcue_config::import::sqlitedict::PendingTable::closed_handle(&dir.profiles_file(), "unnamed")
            });
        }
        if let Some(table) = &self.profile_db {
            if let Err(error) = table.close() {
                // The caller logs this failure. The connection
                // is retained when a normal close fails; do not force-drop it.
                tracing::debug!(%error, "Profile database close raised");
                return;
            }
        }
        self.profile_views_dirty = true;
    }

    #[cfg(test)]
    pub(crate) fn profile_fixture_sql(&self, sql: &str) {
        self.profile_db.as_ref().expect("fixture has an open profile database").execute_batch(sql).unwrap();
    }

    /// [`ConfigStore::commit_games`] for one game.
    pub fn commit_game(&mut self, game: &str, changed: Configuration) -> Result<(), StoreError> {
        self.commit_games([game], changed)
    }

    /// Store `changed` as the configuration once its speaker layouts are written: the speaker layout table
    /// is autocommitted, so a duplicate, a removal and the profile Save's
    /// layout assignment are committed at once. On a write failure the
    /// configuration in memory is left unchanged.
    pub fn commit_layouts(&mut self, mut changed: Configuration) -> Result<(), StoreError> {
        if let Some(dir) = &self.dir {
            self.persist_first_run()?;
            changed.commit_metadata(dir, "shakers", &self.config)?;
        }
        self.update(|config| *config = changed);
        Ok(())
    }

    /// Replace the joystick profiles and write them at once (the joystick table is autocommitted). On a
    /// write failure the profiles in memory are left unchanged.
    pub fn replace_joysticks(&mut self, profiles: JoystickProfiles) -> Result<(), StoreError> {
        let mut changed = self.config.clone();
        changed.joysticks = profiles;
        if let Some(dir) = &self.dir {
            self.persist_first_run()?;
            changed.commit_metadata(dir, "joystick", &self.config)?;
        }
        self.update(|config| *config = changed);
        Ok(())
    }

    /// One autocommit assignment/deletion in the stored joystick table. An assignment always performs
    /// REPLACE, including an equal value, so an existing row moves to the end of the table's iteration order.
    pub fn set_joystick_entry(
        &mut self,
        key: &str,
        value: Option<openrigcue_config::import::value::LegacyValue>,
    ) -> Result<(), StoreError> {
        if !self.assign_joystick_entry(key, value)? {
            let path =
                self.dir.as_ref().map_or_else(|| std::path::PathBuf::from("metadata.db"), |dir| dir.metadata_file());
            return Err(StoreError::Parse { path, message: format!("KeyError: {key:?}") });
        }
        Ok(())
    }

    pub fn delete_joystick_entry(&mut self, key: &str) -> Result<bool, StoreError> {
        self.assign_joystick_entry(key, None)
    }

    /// Change the notification tables and write them at once (autocommit). The change is applied to
    /// a copy that replaces the tables in memory only once it is on disk, so on a write failure memory and disk
    /// still agree and the next identical change writes again (a row whose commit failed is not kept).
    pub fn update_dismissals(&mut self, change: impl FnOnce(&mut Dismissals)) -> Result<(), StoreError> {
        let mut dismissals = self.config.dismissals.clone();
        change(&mut dismissals);
        if dismissals == self.config.dismissals {
            return Ok(());
        }
        let mut changed = self.config.clone();
        changed.dismissals = dismissals;
        if let Some(dir) = &self.dir {
            self.persist_first_run()?;
            changed.commit_metadata(dir, "game_config_notifications", &self.config)?;
            changed.commit_metadata(dir, "notifications", &self.config)?;
        }
        self.update(|config| *config = changed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_dictionaries_survive_unlink_and_share_the_startup_cache_worker() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        for version in [411, 412] {
            let (_temporary, dir, old_store) = saved_store();
            drop(old_store);
            let mut raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
            raw.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", version.into());
            Settings::from_legacy(&raw).0.write_ini_only(&dir.settings_file()).unwrap();
            for table in ["joystick", "shakers", "game_config_notifications", "notifications", "cache"] {
                sqlitedict::set_item(&dir.metadata_file(), table, "OnlyOnDisk", &LegacyValue::Int(37)).unwrap();
            }
            let mut store = ConfigStore::open(dir.clone(), Platform::Linux, None).unwrap();
            let cache = crate::accessories::PortCache::from_open_table(
                &dir.metadata_file(),
                store.port_cache_database().unwrap(),
                None,
            )
            .unwrap();
            std::fs::remove_file(dir.profiles_file()).unwrap();
            std::fs::remove_file(dir.metadata_file()).unwrap();
            assert!(store.profile_row("ac").unwrap().is_some());
            for table in ["joystick", "shakers", "game_config_notifications", "notifications", "cache"] {
                assert_eq!(
                    store.metadata_row(table, "OnlyOnDisk").unwrap(),
                    Some(LegacyValue::Int(37)),
                    "schema {version}: {table}"
                );
            }
            cache.close().unwrap();
            assert!(store.metadata_row("cache", "OnlyOnDisk").is_err(), "PortCache owns the same dictionary");
            store.close_opened_files();
            assert!(store.metadata_row("joystick", "OnlyOnDisk").is_err());
            assert_eq!(
                store.metadata_row("game_config_notifications", "OnlyOnDisk").unwrap(),
                Some(LegacyValue::Int(37))
            );
            store.close_launcher_files();
            assert!(store.metadata_row("game_config_notifications", "OnlyOnDisk").is_err());
            assert!(!dir.profiles_file().exists());
            assert!(!dir.metadata_file().exists());
        }
    }

    #[test]
    fn port_forwarding_missing_stored_file_starts_with_an_empty_configobj() {
        let (_temporary, dir, mut store) = saved_store();
        store.update(|config| {
            config.settings.app.language = "unsaved-language".into();
            config.settings.port_forwarding.insert("fixture".into(), vec!["127.0.0.1:1234".into()]);
        });
        let companions = [dir.profiles_file(), dir.metadata_file(), dir.registry_file()];
        let before = companions.each_ref().map(|path| std::fs::read(path).unwrap());
        std::fs::remove_file(dir.settings_file()).unwrap();
        store.write_port_forwarding().unwrap();
        let text = std::fs::read_to_string(dir.settings_file()).unwrap();
        let raw = openrigcue_config::import::configobj::parse(&text).unwrap();
        assert_eq!(raw.len(), 1, "a fresh ConfigObj contains only the copied section: {text}");
        assert_eq!(raw.get("PORT_FORWARDING"), store.settings().native_snapshot().get("PORT_FORWARDING"));
        assert_eq!(companions.each_ref().map(|path| std::fs::read(path).unwrap()), before);
        assert_eq!(store.settings().app.language, "unsaved-language");
    }

    #[test]
    fn port_forwarding_copies_the_native_section_without_the_typed_projection() {
        use openrigcue_config::import::value::Dict;
        let (_temporary, dir, mut store) = saved_store();
        let mut forwarding = Dict::new();
        forwarding.insert("single", LegacyValue::Str("127.0.0.1:4321".into()));
        forwarding.insert("targets", LegacyValue::List(vec!["first:1".into(), "second:2".into()]));
        for native in [LegacyValue::Dict(forwarding), LegacyValue::Str("native-scalar".into()), LegacyValue::None] {
            store.update(|config| {
                config.settings.edit_native(|raw| raw.insert("PORT_FORWARDING", native.clone()));
            });
            store.write_port_forwarding().unwrap();
            let text = std::fs::read_to_string(dir.settings_file()).unwrap();
            let raw = openrigcue_config::import::configobj::parse(&text).unwrap();
            let expected = if native == LegacyValue::None { LegacyValue::Str("None".into()) } else { native };
            assert_eq!(raw.get("PORT_FORWARDING"), Some(&expected));
            assert!(raw.contains("APP"), "the rest of the stored file must remain");
        }
    }

    #[test]
    fn port_forwarding_absent_live_section_leaves_the_stored_section_in_place() {
        let (_temporary, dir, mut store) = saved_store();
        let mut stored = Settings::load_ini_for_native_edit(&dir.settings_file()).unwrap();
        stored.edit_native(|raw| raw.insert("PORT_FORWARDING", LegacyValue::Str("stored-only".into())));
        stored.write_ini_only(&dir.settings_file()).unwrap();
        store.update(|config| {
            config.settings.edit_native(|raw| {
                raw.remove("PORT_FORWARDING");
            });
        });
        store.write_port_forwarding().unwrap();
        let raw = Settings::load_ini_for_native_edit(&dir.settings_file()).unwrap().native_snapshot();
        assert_eq!(raw.get("PORT_FORWARDING"), Some(&LegacyValue::Str("stored-only".into())));
        assert!(!store.settings().native_snapshot().contains("PORT_FORWARDING"));
    }

    #[test]
    fn native_ini_writes_preserve_independently_changed_launcher_flags() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let (_temporary, dir, mut store) = saved_store();
        for key in ["StartWithWindows", "ShowSplashScreen"] {
            sqlitedict::set_item(&dir.registry_file(), "registry", key, &LegacyValue::from("0")).unwrap();
        }
        let before = std::fs::read(dir.registry_file()).unwrap();
        store.update(|config| config.settings.app.start_page = "Motion".into());
        store.write_settings_native();
        assert_eq!(Settings::load(&dir.settings_file()).unwrap().app.start_page, "Motion");
        assert!(std::fs::read(dir.registry_file()).unwrap() == before, "native INI write overwrote external flags");
        store.write_setting(|settings| settings.app.hide_standby_tuning_alert = true).unwrap();
        store.write_native_setting("APP", "disable_tuning_alert_standby", &LegacyValue::Str("yes".into())).unwrap();
        store.update(|config| config.settings.port_forwarding.insert("fixture".into(), vec!["127.0.0.1:1234".into()]));
        store.write_port_forwarding().unwrap();
        assert!(std::fs::read(dir.registry_file()).unwrap() == before);
    }

    /// Only the start that creates the configuration is a first installation; a language chosen then is kept.
    #[test]
    fn first_installation_language_is_saved_and_later_starts_are_not_first() {
        let temporary = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(temporary.path().join("cfg"));
        std::fs::create_dir_all(dir.root()).unwrap();
        let mut store = ConfigStore::open(dir.clone(), Platform::Windows, None).unwrap();
        assert!(store.first_installation());
        assert_eq!(store.settings().app.language, "en");
        store.update(|config| config.settings.app.language = "de".into());
        store.write_settings_native();
        drop(store);
        let store = ConfigStore::open(dir, Platform::Windows, None).unwrap();
        assert!(!store.first_installation());
        assert_eq!(store.settings().app.language, "de");
    }

    #[test]
    fn runtime_ini_write_does_not_reset_or_repair_an_unreadable_launcher_flag() {
        let (_temporary, dir, old_store) = saved_store();
        drop(old_store);
        let connection = rusqlite::Connection::open(dir.registry_file()).unwrap();
        connection.execute("UPDATE registry SET value=? WHERE key='StartWithWindows'", [vec![255_u8]]).unwrap();
        drop(connection);
        let before = std::fs::read(dir.registry_file()).unwrap();
        let mut store = ConfigStore::open(dir.clone(), Platform::Windows, None).unwrap();
        assert!(
            store.take_start_info().reset.is_none(),
            "config_helpers.write cannot reset because of a registry value"
        );
        store.write_settings_native();
        store.write_setting(|settings| settings.app.hide_standby_tuning_alert = true).unwrap();
        store.write_native_setting("APP", "disable_tuning_alert_standby", &LegacyValue::Str("yes".into())).unwrap();
        store.write_port_forwarding().unwrap();
        assert!(std::fs::read(dir.registry_file()).unwrap() == before);
        assert!(!dir.root().join("reset.txt").exists());
    }

    #[test]
    fn runtime_shaker_startup_requires_the_stored_profile_name() {
        let (_temporary, dir, mut store) = saved_store();
        store.update(|config| {
            config.settings.edit_native(|raw| {
                raw.get_mut("SHAKEBOX").unwrap().as_dict_mut().unwrap().remove("profile");
            })
        });
        store.save().unwrap();
        drop(store);
        let loaded = Settings::load(&dir.settings_file()).unwrap();
        assert!(loaded.native_snapshot().get("SHAKEBOX").unwrap().item("profile").is_err());
        assert!(
            ConfigStore::open(dir, Platform::Linux, None).is_err(),
            "missing native profile must not use the typed default"
        );
    }

    #[test]
    fn runtime_shaker_startup_reads_only_the_selected_raw_value_and_reload_keeps_failures_atomic() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        for value in [
            LegacyValue::None,
            LegacyValue::Dict(Default::default()),
            LegacyValue::List(Vec::new()),
            LegacyValue::Int(7),
        ] {
            let (_temporary, dir, old) = saved_store();
            let name = old.settings().shakers.speaker_layout.clone();
            drop(old);
            sqlitedict::set_item(&dir.metadata_file(), "shakers", &name, &value).unwrap();
            let connection = rusqlite::Connection::open(dir.metadata_file()).unwrap();
            connection.execute("INSERT INTO shakers (key,value) VALUES ('opaque-other',?)", [vec![255_u8]]).unwrap();
            drop(connection);
            let before = std::fs::read(dir.metadata_file()).unwrap();
            let mut store = ConfigStore::open(dir.clone(), Platform::Windows, None).unwrap();
            assert!(store.take_start_info().reset.is_none(), "decoded selected objects are not missing layouts");
            assert_eq!(store.config().metadata_table("shakers").get(&name), Some(&value));
            assert_eq!(std::fs::read(dir.metadata_file()).unwrap(), before);
            let state = crate::state::ServiceState::new(store, crate::state::OriginalPlatform::Windows);
            assert_eq!(state.shaker_layout.read().value, value);
            let updated = LegacyValue::from(serde_json::json!({"FL":{"enable":true},"untyped":null}));
            sqlitedict::set_item(&dir.metadata_file(), "shakers", &name, &updated).unwrap();
            crate::shaker_layout::load(&state).unwrap();
            assert_eq!(state.shaker_layout.read().value, updated);
            let connection = rusqlite::Connection::open(dir.metadata_file()).unwrap();
            connection
                .execute("UPDATE shakers SET value=? WHERE key=?", rusqlite::params![vec![255_u8], name])
                .unwrap();
            assert!(crate::shaker_layout::load(&state).is_err());
            assert_eq!(state.shaker_layout.read().value, updated, "failed decode cannot replace the global snapshot");
            state.config.write().close_opened_files();
            assert!(!dir.root().join("reset.txt").exists());
        }
    }

    #[test]
    fn runtime_start_defers_joystick_values_until_canonical_consumers_read_them() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        for platform in [Platform::Windows, Platform::Linux] {
            let (_temporary, dir, old_store) = saved_store();
            drop(old_store);
            let default = sqlitedict::read_item(&dir.metadata_file(), "joystick", "Default").unwrap().unwrap();
            let connection = rusqlite::Connection::open(dir.metadata_file()).unwrap();
            connection.execute("INSERT INTO joystick (key,value) VALUES ('opaque',?)", [vec![255_u8]]).unwrap();
            drop(connection);
            let before = std::fs::read(dir.metadata_file()).unwrap();
            let mut store = ConfigStore::open(dir.clone(), platform, None).unwrap();
            assert!(store.take_start_info().reset.is_none(), "unrelated joystick value reset the folder");
            assert!(store.config().joysticks.0.is_empty(), "the runtime snapshot must not eagerly decode rows");
            assert_eq!(store.metadata_row("joystick", "Default").unwrap(), Some(default.clone()));
            assert_eq!(
                store.metadata_keys("joystick").unwrap().collect::<Result<Vec<_>, _>>().unwrap(),
                ["Default", "opaque"]
            );
            assert!(store.metadata_row("joystick", "opaque").is_err());
            let mut rows = store.metadata_rows("joystick").unwrap();
            assert_eq!(rows.next().unwrap().unwrap(), ("Default".into(), default.clone()));
            assert!(rows.next().unwrap().is_err(), "GUI items() still fails when it reaches the bad row");
            drop(rows);
            store.write_settings_native();
            assert_eq!(std::fs::read(dir.metadata_file()).unwrap(), before);
            assert!(Configuration::load(&dir).is_err(), "explicit complete snapshots still decode every row");
            store.set_joystick_entry("Fresh", Some(LegacyValue::Dict(Default::default()))).unwrap();
            assert_eq!(store.metadata_row("joystick", "Default").unwrap(), Some(default));
            assert!(store.metadata_row("joystick", "Fresh").unwrap().is_some());
            store.close_opened_files();
            let opaque: Vec<u8> = rusqlite::Connection::open(dir.metadata_file())
                .unwrap()
                .query_row("SELECT value FROM joystick WHERE key='opaque'", [], |row| row.get(0))
                .unwrap();
            assert_eq!(opaque, [255]);
            assert!(!dir.root().join("reset.txt").exists());
        }
    }

    #[test]
    fn runtime_start_defers_notification_values_and_preserves_opaque_rows() {
        let (_temporary, dir, old_store) = saved_store();
        drop(old_store);
        let connection = rusqlite::Connection::open(dir.metadata_file()).unwrap();
        for table in ["notifications", "game_config_notifications"] {
            connection
                .execute(&format!("INSERT INTO {table} (key,value) VALUES ('opaque',?)"), [vec![255_u8]])
                .unwrap();
        }
        drop(connection);
        let before = std::fs::read(dir.metadata_file()).unwrap();
        for platform in [Platform::Linux, Platform::Windows] {
            let mut store = ConfigStore::open(dir.clone(), platform, None).unwrap();
            assert!(store.take_start_info().reset.is_none());
            assert!(store.config().dismissals.game_setup.is_empty());
            assert!(store.config().dismissals.notification_keys.is_empty());
            for table in ["notifications", "game_config_notifications"] {
                assert_eq!(store.metadata_keys(table).unwrap().collect::<Result<Vec<_>, _>>().unwrap(), ["opaque"]);
                assert!(store.metadata_row(table, "opaque").is_err(), "decoding belongs to the actual selected read");
            }
            store.save().unwrap();
            store.close_opened_files();
            assert!(std::fs::read(dir.metadata_file()).unwrap() == before);
        }
        assert!(!dir.root().join("reset.txt").exists());
    }

    #[test]
    fn shaker_worker_keeps_its_original_database_after_path_replacement() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let (_temporary, directory, mut store) = saved_store();
        let name = store.settings().shakers.speaker_layout.clone();
        let original = store.metadata_row("shakers", &name).unwrap().unwrap();
        let path = directory.metadata_file();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(store.set_shaker_entry(&name, original.clone()).is_err());
        std::fs::remove_dir(&path).unwrap();
        let replacement = LegacyValue::from(serde_json::json!({"replacement":true}));
        sqlitedict::set_item(&path, "shakers", &name, &replacement).unwrap();
        assert_eq!(store.metadata_row("shakers", &name).unwrap(), Some(original.clone()));
        assert!(
            store.set_shaker_entry(&name, original).is_err(),
            "a new pathname does not reopen the native dictionary"
        );
        assert_eq!(sqlitedict::read_item(&path, "shakers", &name).unwrap(), Some(replacement));
        store.close_opened_files();
    }

    #[test]
    fn notification_markers_always_autocommit_replace_even_when_already_cached() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let (_temporary, dir, mut store) = saved_store();
        store.record_notification_marker("notifications", "one").unwrap();
        store.record_notification_marker("notifications", "two").unwrap();
        let revision = store.revision();
        let connection = rusqlite::Connection::open(dir.metadata_file()).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_marker BEFORE INSERT ON notifications BEGIN SELECT RAISE(ABORT,'fixture marker failure'); END;").unwrap();
        assert!(
            store.record_notification_marker("notifications", "one").is_err(),
            "a cached True cannot skip the SQL assignment"
        );
        assert_eq!(store.revision(), revision);
        connection.execute_batch("DROP TRIGGER reject_marker").unwrap();
        store.record_notification_marker("notifications", "one").unwrap();
        assert_eq!(store.revision(), revision, "unchanged settings need no derived-view rebuild");
        assert_eq!(
            store.metadata_keys("notifications").unwrap().collect::<Result<Vec<_>, _>>().unwrap(),
            ["two", "one"]
        );
        connection.execute("INSERT INTO notifications (key,value) VALUES ('opaque',?)", [vec![255_u8]]).unwrap();
        drop(connection);
        sqlitedict::set_item(&dir.metadata_file(), "notifications", "one", &LegacyValue::None).unwrap();
        store.record_notification_marker("notifications", "one").unwrap();
        assert_eq!(store.metadata_row("notifications", "one").unwrap(), Some(LegacyValue::Bool(true)));
        assert_eq!(
            store.metadata_keys("notifications").unwrap().collect::<Result<Vec<_>, _>>().unwrap(),
            ["two", "opaque", "one"]
        );
        assert_eq!(store.config().metadata_table("notifications").get("one"), Some(&LegacyValue::Bool(true)));
    }

    #[test]
    fn settings_save_after_ini_deletion_preserves_live_databases() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let (_temporary, dir, old_store) = saved_store();
        drop(old_store);
        let mut store = ConfigStore::open(dir.clone(), Platform::Linux, None).unwrap();
        assert_eq!(store.config().profiles.games().count(), 1);
        let connection = rusqlite::Connection::open(dir.profiles_file()).unwrap();
        connection.execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![255_u8]]).unwrap();
        drop(connection);
        sqlitedict::set_item(&dir.metadata_file(), "joystick", "External", &LegacyValue::None).unwrap();
        let profiles = std::fs::read(dir.profiles_file()).unwrap();
        let metadata = std::fs::read(dir.metadata_file()).unwrap();
        std::fs::remove_file(dir.settings_file()).unwrap();
        store.update(|config| config.settings.app.start_page = "Motion".into());
        store.save().unwrap();
        assert!(std::fs::read(dir.profiles_file()).unwrap() == profiles, "settings save rewrote profile.db");
        assert!(std::fs::read(dir.metadata_file()).unwrap() == metadata, "settings save rewrote metadata.db");
        assert_eq!(Settings::load(&dir.settings_file()).unwrap().app.start_page, "Motion");
    }

    #[test]
    fn settings_save_after_ini_deletion_leaves_profile_transaction_pending() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let (_temporary, dir, mut store) = saved_store();
        let original = store.profile_row("emulator").unwrap();
        let pending = LegacyValue::from(serde_json::json!({"gear_shift_perc":73}));
        store.stage_game_row("emulator", Some(pending.clone())).unwrap();
        assert_eq!(store.profile_row("emulator").unwrap(), Some(pending.clone()));
        std::fs::remove_file(dir.settings_file()).unwrap();
        store.save().unwrap();
        assert_eq!(store.profile_row("emulator").unwrap(), Some(pending));
        assert!(sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap() == original);
        store.close_profile_database();
        assert!(store.profile_row("emulator").is_err(), "closed dictionary must stay closed");
        assert!(sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap() == original);
    }

    #[test]
    fn ini_deletion_keeps_snapshot_and_runtime_dictionary_reads_canonical() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let (_temporary, dir, old_store) = saved_store();
        drop(old_store);
        for runtime in [false, true] {
            let mut store = if runtime {
                ConfigStore::open(dir.clone(), Platform::Linux, None).unwrap()
            } else {
                ConfigStore::load(dir.clone()).unwrap()
            };
            assert_eq!(store.metadata_db.get_mut().unwrap().is_empty(), !runtime);
            let live = LegacyValue::from(serde_json::json!({"external":runtime}));
            sqlitedict::set_item(&dir.metadata_file(), "joystick", "External", &live).unwrap();
            sqlitedict::set_item(&dir.profiles_file(), "unnamed", "external", &live).unwrap();
            std::fs::remove_file(dir.settings_file()).unwrap();
            assert_eq!(store.metadata_row("joystick", "External").unwrap(), Some(live.clone()));
            assert!(store.metadata_contains("joystick", "External").unwrap());
            assert!(store
                .metadata_rows("joystick")
                .unwrap()
                .any(|row| row.unwrap() == ("External".into(), live.clone())));
            assert_eq!(store.profile_row("external").unwrap(), Some(live.clone()));
            assert!(store.profile_rows().unwrap().any(|row| row.unwrap() == ("external".into(), live.clone())));
            store.save().unwrap();
        }
    }

    #[test]
    fn metadata_reads_use_current_rows_and_keep_late_decode_errors_lazy() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        let (_temporary, dir, mut store) = saved_store();
        let before = store.config().metadata_table("joystick");
        let live = LegacyValue::from(serde_json::json!({"external":17}));
        sqlitedict::set_item(&dir.metadata_file(), "joystick", "Default", &live).unwrap();
        let connection = rusqlite::Connection::open(dir.metadata_file()).unwrap();
        connection.execute("INSERT INTO joystick (key,value) VALUES ('opaque',?)", [vec![255_u8]]).unwrap();
        drop(connection);
        sqlitedict::set_item(&dir.metadata_file(), "joystick", "Later", &LegacyValue::None).unwrap();
        assert_eq!(store.config().metadata_table("joystick"), before, "compatibility view is deliberately stale");
        assert_eq!(store.metadata_row("joystick", "Default").unwrap(), Some(live.clone()));
        assert!(store.metadata_contains("joystick", "opaque").unwrap());
        assert!(store.metadata_row("joystick", "opaque").is_err());
        let mut rows = store.metadata_rows("joystick").unwrap();
        assert_eq!(rows.next().unwrap().unwrap(), ("Default".into(), live));
        assert!(rows.next().unwrap().is_err());
        assert!(rows.next().is_none());
        let revision = store.revision();
        assert!(!store.delete_joystick_entry("absent").unwrap());
        assert_eq!(store.revision(), revision);
        assert!(store.delete_joystick_entry("opaque").unwrap(), "deletion tests membership without decoding");
        store.set_joystick_entry("New", Some(LegacyValue::None)).unwrap();
        assert_eq!(sqlitedict::read_item(&dir.metadata_file(), "joystick", "New").unwrap(), Some(LegacyValue::None));
        assert_eq!(store.metadata_table("joystick").unwrap().str_keys(), ["Default", "Later", "New"]);
        store.close_opened_files();
        assert!(store.metadata_db.get_mut().unwrap().contains_key("joystick"));
        assert!(store.metadata_row("joystick", "Default").is_err());
    }

    #[test]
    fn runtime_start_keeps_open_profile_worker_and_lazily_reads_other_games() {
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        use openrigcue_config::profiles::Vehicle;
        let (_temporary, dir, old_store) = saved_store();
        let original = old_store.profile_row("ac").unwrap().unwrap();
        drop(old_store);
        let path = dir.profiles_file();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![255_u8]]).unwrap();
        drop(connection);
        let mut store = ConfigStore::open(dir.clone(), Platform::Linux, None).unwrap();
        assert!(store.profile_db.is_some(), "worker opens at startup, before the first edit");
        assert_eq!(
            store.config().profiles.games().count(),
            1,
            "only the startup emulator compatibility row was loaded"
        );
        assert!(store.profile_row("ac").unwrap() == Some(original.clone()));
        assert!(store.profile_row("opaque").is_err());
        let mut live = original.clone();
        live.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(77));
        sqlitedict::set_item(&path, "unnamed", "ac", &live).unwrap();
        let profile = crate::profile::ActiveProfile::load(&store, "ac", &Vehicle::Unknown, 1).unwrap();
        assert_eq!(profile.tune.gear_shift_perc, 77.0);
        store.stage_game_row("ac", Some(original)).unwrap();
        store.close_profile_database();
        assert!(store.profile_db.is_some(), "the closed native dictionary object remains retained");
        assert!(store.profile_row("ac").is_err());
        assert!(sqlitedict::read_item(&path, "unnamed", "ac").unwrap() == Some(live));
    }

    #[test]
    fn live_profile_reads_retain_current_pending_and_rolled_back_rows() {
        use crate::profile::ActiveProfile;
        use openrigcue_config::import::{sqlitedict, value::LegacyValue};
        use openrigcue_config::profiles::Vehicle;
        let (_tmp, dir, mut store) = saved_store();
        let initial = store.profile_row("emulator").unwrap().unwrap();
        let mut live = initial.clone();
        live.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(99));
        let mut vehicle = initial.clone();
        vehicle.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(43));
        live.as_dict_mut().unwrap().get_mut("vehicles").unwrap().as_dict_mut().unwrap().insert("fixture car", vehicle);
        live.as_dict_mut()
            .unwrap()
            .get_mut("vehicles")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .insert("null car", LegacyValue::None);
        sqlitedict::set_item(&dir.profiles_file(), "unnamed", "emulator", &live).unwrap();
        let database = rusqlite::Connection::open(dir.profiles_file()).unwrap();
        database.execute("INSERT INTO unnamed (key,value) VALUES ('opaque',?)", [vec![0xff_u8]]).unwrap();
        drop(database);
        assert_eq!(store.profile_row("emulator").unwrap(), Some(live.clone()));
        assert_eq!(store.profile_row("absent").unwrap(), None);
        assert!(store.profile_row("opaque").is_err());
        let profile = ActiveProfile::load(&store, "none", &Vehicle::Unknown, 7).unwrap();
        assert_eq!(profile.native_tune().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(99));
        let profile = ActiveProfile::load(&store, "emulator", &Vehicle::Named("fixture car".into()), 8).unwrap();
        assert_eq!(profile.native_tune().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(43));
        let null_profile = ActiveProfile::load(&store, "emulator", &Vehicle::Named("null car".into()), 8).unwrap();
        assert_eq!(null_profile.native_tune(), LegacyValue::None);
        assert_eq!(null_profile.tune.gear_shift_perc, 99.0, "compatibility fallback comes from the current row");
        let mut pending = live.clone();
        pending.as_dict_mut().unwrap().insert("gear_shift_perc", LegacyValue::Int(73));
        store.stage_game_row("emulator", Some(pending.clone())).unwrap();
        assert_eq!(store.profile_row("emulator").unwrap(), Some(pending));
        assert_eq!(sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap(), Some(live.clone()));
        assert!(store.profile_row("opaque").is_err());
        store.close_profile_database();
        assert!(store.profile_row("emulator").is_err());
        assert_eq!(sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap(), Some(live));
        let restarted = ConfigStore::open(dir, Platform::Linux, None).unwrap();
        let profile = ActiveProfile::load(&restarted, "none", &Vehicle::Unknown, 9).unwrap();
        assert_eq!(profile.native_tune().item_owned("gear_shift_perc").unwrap(), LegacyValue::Int(99));
    }

    use super::*;

    /// The settings are only written on Save: in-memory changes do not touch disk until `save`.
    #[test]
    fn changes_stay_in_memory_until_save() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path().join("openrigcue"));
        let mut store = ConfigStore::load(dir.clone()).unwrap();
        store.update(|c| c.settings.app.web_ui_port = 40000);
        assert_eq!(store.revision(), 1);
        assert!(!dir.settings_file().exists());
        store.save().unwrap();
        let reloaded = ConfigStore::load(dir).unwrap();
        assert_eq!(reloaded.settings().app.web_ui_port, 40000);
    }

    #[test]
    fn settings_save_does_not_rewrite_profile_or_metadata_rows() {
        let (_tmp, dir, mut store) = saved_store();
        let profiles = std::fs::read(dir.profiles_file()).unwrap();
        let metadata = std::fs::read(dir.metadata_file()).unwrap();
        store.update(|config| config.settings.app.start_page = "Motion".into());
        store.save().unwrap();
        assert_eq!(std::fs::read(dir.profiles_file()).unwrap(), profiles);
        assert_eq!(std::fs::read(dir.metadata_file()).unwrap(), metadata);
        assert_eq!(Settings::load(&dir.settings_file()).unwrap().app.start_page, "Motion");
    }

    /// metadata.db tables are autocommit: a dismissal is written immediately, an unchanged set is not.
    #[test]
    fn dismissals_are_written_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path().join("openrigcue"));
        let mut store = ConfigStore::load(dir.clone()).unwrap();
        store.update_dismissals(|d| d.record_telemetry_received("dirt")).unwrap();
        assert!(dir.metadata_file().exists());
        let revision = store.revision();
        store.update_dismissals(|d| d.record_telemetry_received("dirt")).unwrap();
        assert_eq!(store.revision(), revision);
        let back = Configuration::load(&dir).unwrap().dismissals;
        assert!(back.setup_instructions_suppressed("dirt"));
    }

    /// A failed dismissal write leaves the tables in memory unchanged, so the same dismissal is written by the
    /// next call once the file can be written.
    #[test]
    fn failed_dismissal_write_is_retried_by_the_next_call() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path().join("openrigcue"));
        let mut store = ConfigStore::load(dir.clone()).unwrap();
        store.save().unwrap();
        std::fs::remove_file(dir.metadata_file()).unwrap();
        std::fs::create_dir(dir.metadata_file()).unwrap();
        let revision = store.revision();
        assert!(store.update_dismissals(|d| d.dismiss_game_setup("dirt")).is_err());
        assert!(!store.config().dismissals.game_setup_dismissed("dirt"));
        assert_eq!(store.revision(), revision);

        std::fs::remove_dir(dir.metadata_file()).unwrap();
        store.update_dismissals(|d| d.dismiss_game_setup("dirt")).unwrap();
        assert!(store.config().dismissals.game_setup_dismissed("dirt"));
        assert_eq!(store.revision(), revision + 1);
        let restarted = ConfigStore::load(dir).unwrap();
        assert!(restarted.config().dismissals.game_setup_dismissed("dirt"));
    }

    /// A store on a fresh folder (nothing written yet) whose first write is `write`; returns the folder.
    fn first_write_on_fresh_folder(write: impl FnOnce(&mut ConfigStore)) -> (tempfile::TempDir, ConfigDir) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path().join("openrigcue"));
        let mut store = ConfigStore::load(dir.clone()).unwrap();
        assert!(!dir.root().exists());
        write(&mut store);
        (tmp, dir)
    }

    /// The folder after a first single-file write holds the whole configuration: the first-run settings and
    /// every companion file, so the next start loads them instead of starting over.
    fn assert_complete_folder(dir: &ConfigDir) {
        assert!(dir.settings_file().exists());
        assert!(dir.metadata_file().exists());
        assert_eq!(
            openrigcue_config::import::sqlitedict::read_table(&dir.profiles_file(), "unnamed").unwrap().len(),
            Configuration::first_run().profiles.native_table().len()
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// First notification dismissal on a never-saved folder: kept
    /// at the next start, with the first-run settings.
    #[test]
    fn first_dismissal_on_a_fresh_folder_survives_a_restart() {
        let (_tmp, dir) = first_write_on_fresh_folder(|store| {
            store.update(|c| c.settings.app.web_ui_port = 40000);
            store.update_dismissals(|d| d.dismiss_game_setup("dirt")).unwrap();
        });
        assert_complete_folder(&dir);
        let restarted = ConfigStore::load(dir).unwrap();
        assert!(restarted.config().dismissals.game_setup_dismissed("dirt"));
        // An unsaved settings edit is not written by an autocommit of another table.
        assert_eq!(restarted.settings(), &Configuration::first_run().settings);
    }

    /// First tune commit (`commit_game` alone) on a never-saved folder survives a restart; the other games keep
    /// their first-run tunes.
    #[test]
    fn first_game_save_on_a_fresh_folder_survives_a_restart() {
        let first_run = Configuration::first_run();
        let (game, tune) = first_run.profiles.games().next().map(|(g, t)| (g.to_owned(), t.clone())).unwrap();
        let mut edited = tune.clone();
        edited.gear_shift_perc = tune.gear_shift_perc + 1.5;
        let (_tmp, dir) = first_write_on_fresh_folder(|store| {
            let mut changed = store.config().clone();
            changed.profiles.insert(&game, edited.clone());
            store.commit_game(&game, changed).unwrap();
        });
        assert_complete_folder(&dir);
        let restarted = ConfigStore::load(dir).unwrap();
        assert_eq!(restarted.config().profiles.get(&game), Some(&edited));
        let others = |c: &Configuration| {
            c.profiles.games().filter(|(g, _)| *g != game).map(|(g, t)| (g.to_owned(), t.clone())).collect::<Vec<_>>()
        };
        assert_eq!(others(restarted.config()), others(&first_run));
    }

    /// First joystick profile autocommit on a never-saved folder survives a restart.
    #[test]
    fn first_joystick_write_on_a_fresh_folder_survives_a_restart() {
        let mut profiles = Configuration::first_run().joysticks;
        let copy = profiles.0["Default"].clone();
        profiles.0.insert("Wheel".into(), copy);
        let (_tmp, dir) = first_write_on_fresh_folder(|store| {
            store.replace_joysticks(profiles.clone()).unwrap();
            assert_eq!(store.config().joysticks, profiles);
        });
        assert_complete_folder(&dir);
        let restarted = ConfigStore::load(dir).unwrap();
        assert_eq!(restarted.config().joysticks, profiles);
    }

    /// On a saved folder a single-file write touches only its own file; the saved settings stay.
    #[test]
    fn single_file_writes_on_a_saved_folder_keep_the_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path().join("openrigcue"));
        let mut store = ConfigStore::load(dir.clone()).unwrap();
        store.update(|c| c.settings.app.web_ui_port = 40000);
        store.save().unwrap();
        store.update(|c| c.settings.app.web_ui_port = 40001);
        store.update_dismissals(|d| d.dismiss_game_setup("dirt")).unwrap();
        let game = store.config().profiles.games().next().map(|(g, _)| g.to_owned()).unwrap();
        let unchanged = store.config().clone();
        store.commit_game(&game, unchanged).unwrap();
        let restarted = ConfigStore::load(dir).unwrap();
        assert_eq!(restarted.settings().app.web_ui_port, 40000);
        assert!(restarted.config().dismissals.game_setup_dismissed("dirt"));
    }

    /// A saved folder and its store, for the commit tests.
    fn saved_store() -> (tempfile::TempDir, ConfigDir, ConfigStore) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path().join("openrigcue"));
        let store = ConfigStore::load(dir.clone()).unwrap();
        store.save().unwrap();
        // The fixture represents a store after start-up has built its cache.
        openrigcue_config::import::sqlitedict::ensure_table(&dir.metadata_file(), "cache").unwrap();
        (tmp, dir, store)
    }

    /// The first two games of the first run and a copy of the configuration with both tunes edited.
    fn two_games_edited(store: &ConfigStore) -> (String, String, Configuration) {
        let mut changed = store.config().clone();
        let games: Vec<String> = changed.profiles.games().take(2).map(|(g, _)| g.to_owned()).collect();
        for game in &games {
            let mut tune = changed.profiles.get(game).unwrap().clone();
            tune.gear_shift_perc += 1.5;
            changed.profiles.insert(game, tune);
        }
        (games[0].clone(), games[1].clone(), changed)
    }

    /// A layout assignment on a saved folder (autocommit): the layout is kept at the next start
    /// without a full Save.
    #[test]
    fn committed_layout_on_a_saved_folder_survives_a_restart() {
        let (_tmp, dir, mut store) = saved_store();
        let mut changed = store.config().clone();
        let mut layout = changed.shaker_layouts.get("6 Speakers - 5.1").unwrap().clone();
        layout.subwoofer.sfx.accel = 7.0;
        changed.shaker_layouts.set("Mine", layout.clone());
        let revision = store.revision();
        store.commit_layouts(changed).unwrap();
        assert_eq!(store.revision(), revision + 1);
        assert_eq!(store.config().shaker_layouts.get("Mine"), Some(&layout));
        let restarted = ConfigStore::load(dir).unwrap();
        assert_eq!(restarted.config().shaker_layouts.get("Mine"), Some(&layout));
    }

    /// The first layout commit on a never-saved folder leaves a complete folder that reloads with the layout.
    #[test]
    fn first_layout_commit_on_a_fresh_folder_survives_a_restart() {
        let layout = Configuration::first_run().shaker_layouts.get("6 Speakers - 5.1").unwrap().clone();
        let (_tmp, dir) = first_write_on_fresh_folder(|store| {
            let mut changed = store.config().clone();
            changed.shaker_layouts.set("Mine", layout.clone());
            store.commit_layouts(changed).unwrap();
        });
        assert_complete_folder(&dir);
        let restarted = ConfigStore::load(dir).unwrap();
        assert_eq!(restarted.config().shaker_layouts.get("Mine"), Some(&layout));
        assert_eq!(restarted.settings(), &Configuration::first_run().settings);
    }

    /// A failed layout write keeps nothing in memory (a row whose commit failed is not kept); the
    /// same commit succeeds once the file can be written.
    #[test]
    fn failed_layout_commit_leaves_memory_and_disk_alike() {
        let (_tmp, dir, mut store) = saved_store();
        let before = store.config().clone();
        let mut changed = before.clone();
        changed.shaker_layouts.set("Mine", before.shaker_layouts.get("6 Speakers - 5.1").unwrap().clone());
        changed.settings.shakers.speaker_layout = "Mine".into();
        std::fs::remove_file(dir.metadata_file()).unwrap();
        std::fs::create_dir(dir.metadata_file()).unwrap();
        let revision = store.revision();
        assert!(store.commit_layouts(changed.clone()).is_err());
        assert_eq!((store.config(), store.revision()), (&before, revision));

        std::fs::remove_dir(dir.metadata_file()).unwrap();
        store.commit_layouts(changed).unwrap();
        assert_eq!(store.settings().shakers.speaker_layout, "Mine");
        let restarted = ConfigStore::load(dir).unwrap();
        assert!(restarted.config().shaker_layouts.get("Mine").is_some());
    }

    #[test]
    fn failed_raw_joystick_autocommit_keeps_memory_and_disk_unchanged() {
        use openrigcue_config::import::sqlitedict;
        use openrigcue_config::import::value::LegacyValue;

        let (_tmp, dir, mut store) = saved_store();
        let before = store.config().metadata_table("joystick");
        let revision = store.revision();
        let file = dir.metadata_file();
        let database = rusqlite::Connection::open(&file).unwrap();
        database.execute_batch("CREATE TRIGGER reject_joystick BEFORE INSERT ON joystick BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
        assert!(store.set_joystick_entry("Unassigned", Some(LegacyValue::None)).is_err());
        assert_eq!(store.revision(), revision);
        assert_eq!(store.config().metadata_table("joystick"), before);
        assert_eq!(sqlitedict::read_table(&file, "joystick").unwrap(), before);
        database.execute_batch("DROP TRIGGER reject_joystick").unwrap();
        store.set_joystick_entry("Unassigned", Some(LegacyValue::None)).unwrap();
        let assigned = store.config().metadata_table("joystick");
        let revision = store.revision();
        database.execute_batch("CREATE TRIGGER reject_joystick BEFORE DELETE ON joystick BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
        assert!(store.set_joystick_entry("Unassigned", None).is_err());
        assert_eq!(store.revision(), revision);
        assert_eq!(store.config().metadata_table("joystick"), assigned);
        assert_eq!(sqlitedict::read_table(&file, "joystick").unwrap(), assigned);
    }

    /// A deferred failed assignment cannot become the stored row. A retry
    /// reads the current row rather than the optimistic cached projection.
    #[test]
    fn failed_game_commit_preserves_canonical_row_and_retry_succeeds() {
        let (_tmp, dir, mut store) = saved_store();
        let before = store.config().clone();
        let (game, _, changed) = two_games_edited(&store);
        let file = dir.profiles_file();
        reject_game(&file, &game);
        let revision = store.revision();
        assert!(store.commit_game(&game, changed.clone()).is_err());
        assert!(store.revision() > revision, "the queued assignment returned before SQL failed");
        assert!(store.profile_row(&game).unwrap() == before.profiles.native_table().get(&game).cloned());
        assert!(
            openrigcue_config::import::sqlitedict::read_item(&file, "unnamed", &game).unwrap()
                == before.profiles.native_table().get(&game).cloned()
        );

        store.profile_db.as_ref().unwrap().execute_batch("DROP TRIGGER reject_game").unwrap();
        store.commit_game(&game, changed.clone()).unwrap();
        let restarted = ConfigStore::load(dir).unwrap();
        assert_eq!(restarted.config().profiles.get(&game), changed.profiles.get(&game));
    }

    /// A later SQL error preserves the earlier successful assignment. If the
    /// commit was already queued, it can persist that prefix before delivering
    /// the error; otherwise the prefix stays pending until the next commit.
    #[test]
    fn failed_multi_game_commit_preserves_native_pending_or_committed_prefix() {
        let (_tmp, dir, mut store) = saved_store();
        let before = store.config().clone();
        let (first, second, changed) = two_games_edited(&store);
        reject_game(&dir.profiles_file(), &second);
        assert!(store.commit_games([first.as_str(), second.as_str()], changed.clone()).is_err());
        let old = before.profiles.native_table();
        let edited = changed.profiles.native_table();
        assert!(store.profile_row(&first).unwrap() == edited.get(&first).cloned());
        assert!(store.profile_row(&second).unwrap() == old.get(&second).cloned());
        let restarted = ConfigStore::load(dir).unwrap();
        let disk = restarted.config().profiles.native_table();
        assert!(disk.get(&first) == old.get(&first) || disk.get(&first) == edited.get(&first));
        assert!(disk.get(&second) == old.get(&second));
        store.profile_db.as_ref().unwrap().execute_batch("DROP TRIGGER reject_game").unwrap();
        store.commit_game(&second, changed.clone()).unwrap();
        let restarted = ConfigStore::load(store.dir().unwrap().clone()).unwrap();
        assert_eq!(restarted.config().profiles.get(&first), changed.profiles.get(&first));
        assert_eq!(restarted.config().profiles.get(&second), changed.profiles.get(&second));
    }

    #[test]
    fn saving_another_game_commits_pending_imports_but_settings_save_does_not() {
        let (_tmp, dir, mut store) = saved_store();
        let before = store.config().profiles.native_table();
        let (first, second, changed) = two_games_edited(&store);
        let imported = changed.profiles.native_table().get(&first).cloned();
        store.stage_game_row(&first, imported).unwrap();
        store.save().unwrap();
        assert_eq!(ConfigStore::load(dir.clone()).unwrap().config().profiles.native_table(), before);
        store.commit_game(&second, changed.clone()).unwrap();
        let reloaded = ConfigStore::load(dir).unwrap();
        assert_eq!(reloaded.config().profiles.get(&first), changed.profiles.get(&first));
        assert_eq!(reloaded.config().profiles.get(&second), changed.profiles.get(&second));
    }

    fn reject_game(database: &std::path::Path, game: &str) {
        rusqlite::Connection::open(database).unwrap().execute_batch(&format!(
            "CREATE TRIGGER reject_game BEFORE INSERT ON unnamed WHEN NEW.key = '{}' BEGIN SELECT RAISE(ABORT, 'test write failure'); END;",
            game.replace('\'', "''"))).unwrap();
    }

    /// A store without a folder never writes.
    #[test]
    fn in_memory_store_writes_nothing() {
        let mut store = ConfigStore::in_memory(Configuration::first_run());
        store.persist_first_run().unwrap();
        let unchanged = store.config().clone();
        store.commit_game("dirt", unchanged).unwrap();
        let unchanged = store.config().clone();
        store.commit_layouts(unchanged).unwrap();
        store.update_dismissals(|d| d.dismiss_game_setup("dirt")).unwrap();
        let profiles = store.config().joysticks.clone();
        store.replace_joysticks(profiles).unwrap();
        assert!(store.dir().is_none());
    }
}

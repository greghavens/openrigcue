//! Metadata table reads and the retained autocommit connections.

use super::*;
use openrigcue_config::import::{
    sqlitedict::AutocommitTable,
    value::{Dict, Key, LegacyValue},
};

impl ConfigStore {
    pub(super) fn metadata_database(&self, table: &str) -> Result<Option<Arc<AutocommitTable>>, StoreError> {
        let Some(dir) = &self.dir else { return Ok(None) };
        let path = dir.metadata_file();
        let mut databases = self.metadata_db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(database) = databases.get(table) {
            return Ok(Some(database.clone()));
        }
        if !self.native_files_initialized() || !path.is_file() {
            return Ok(None);
        }
        let database = Arc::new(
            AutocommitTable::open(&path, table).map_err(|error| StoreError::database(&path, "execute", error))?,
        );
        databases.insert(table.to_owned(), database.clone());
        Ok(Some(database))
    }

    pub fn metadata_row(&self, table: &str, key: &str) -> Result<Option<LegacyValue>, StoreError> {
        if let Some(database) = self.metadata_database(table)? {
            database.get(key).map_err(|error| self.metadata_error("select_one", error))
        } else {
            Ok(self.config.metadata_table(table).get(key).cloned())
        }
    }

    pub fn metadata_contains(&self, table: &str, key: &str) -> Result<bool, StoreError> {
        if let Some(database) = self.metadata_database(table)? {
            database.contains(key).map_err(|error| self.metadata_error("select_one", error))
        } else {
            Ok(self.config.metadata_table(table).contains(key))
        }
    }

    pub fn metadata_rows(&self, table: &str) -> Result<ProfileRows, StoreError> {
        if let Some(database) = self.metadata_database(table)? {
            let path = self.dir.as_ref().expect("disk metadata has a directory").metadata_file();
            let rows = database.rows().map_err(|error| self.metadata_error("select", error))?;
            Ok(Box::new(rows.map(move |row| row.map_err(|error| StoreError::database(&path, "select", error)))))
        } else {
            let rows: Vec<_> = self
                .config
                .metadata_table(table)
                .iter()
                .map(|(key, value)| Ok((key.to_text(), value.clone())))
                .collect();
            Ok(Box::new(rows.into_iter()))
        }
    }

    pub fn metadata_keys(&self, table: &str) -> Result<MetadataKeys, StoreError> {
        if let Some(database) = self.metadata_database(table)? {
            let path = self.dir.as_ref().expect("disk metadata has a directory").metadata_file();
            Ok(Box::new(
                database.keys().map(move |key| key.map_err(|error| StoreError::database(&path, "select", error))),
            ))
        } else {
            let keys: Vec<_> = self.config.metadata_table(table).keys().map(|key| Ok(key.to_text())).collect();
            Ok(Box::new(keys.into_iter()))
        }
    }

    pub fn metadata_table(&self, table: &str) -> Result<Dict, StoreError> {
        self.metadata_rows(table)?.map(|row| row.map(|(key, value)| (Key::from(key), value))).collect()
    }

    fn metadata_error(
        &self,
        operation: &'static str,
        error: openrigcue_config::import::sqlitedict::DbError,
    ) -> StoreError {
        StoreError::database(
            &self.dir.as_ref().expect("disk metadata has a directory").metadata_file(),
            operation,
            error,
        )
    }

    fn writable_metadata_database(&self, table: &str) -> Result<Option<Arc<AutocommitTable>>, StoreError> {
        self.persist_first_run()?;
        if let Some(dir) = &self.dir {
            let opened = self.metadata_db.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(table);
            let path = dir.metadata_file();
            if !opened && !path.is_file() {
                openrigcue_config::import::sqlitedict::ensure_table(&path, table)
                    .map_err(|error| StoreError::database(&path, "execute", error))?;
            }
        }
        self.metadata_database(table)
    }

    /// Recording a marker always writes the row and waits for its autocommit,
    /// even when a cached marker already says true.
    pub(crate) fn record_notification_marker(&mut self, table: &str, key: &str) -> Result<(), StoreError> {
        let value = LegacyValue::Bool(true);
        if let Some(database) = self.writable_metadata_database(table)? {
            database.set(key, &value).map_err(|error| self.metadata_error("execute", error))?;
        }
        let mut cached = self.config.metadata_table(table);
        let changed = cached.get(key) != Some(&value);
        cached.remove(key);
        cached.insert(key, value);
        self.config.replace_native_notifications(table, cached);
        if changed {
            self.revision += 1;
        }
        Ok(())
    }

    pub(super) fn assign_joystick_entry(&mut self, key: &str, value: Option<LegacyValue>) -> Result<bool, StoreError> {
        if let Some(database) = self.writable_metadata_database("joystick")? {
            let result = match &value {
                Some(value) => database.set(key, value),
                None => database.delete(key),
            };
            match result {
                Ok(()) => {}
                Err(openrigcue_config::import::sqlitedict::DbError::MissingKey { .. }) => return Ok(false),
                Err(error) => {
                    return Err(self.metadata_error(if value.is_some() { "execute" } else { "select_one" }, error))
                }
            }
        } else if value.is_none() && !self.config.metadata_table("joystick").contains(key) {
            return Ok(false);
        }
        let mut table = self.config.metadata_table("joystick");
        table.remove(key);
        if let Some(value) = value {
            table.insert(key, value);
        }
        self.update(|configuration| configuration.replace_native_joysticks(table));
        Ok(true)
    }

    /// Storing a speaker layout always writes the raw layout and waits for
    /// its autocommit, even when no matrix value changed.
    pub(crate) fn set_shaker_entry(&mut self, key: &str, value: LegacyValue) -> Result<(), StoreError> {
        if let Some(database) = self.writable_metadata_database("shakers")? {
            database.set(key, &value).map_err(|error| self.metadata_error("execute", error))?;
        }
        let mut table = self.config.metadata_table("shakers");
        table.remove(key);
        table.insert(key, value);
        self.update(|configuration| configuration.replace_native_shakers(table));
        Ok(())
    }

    pub(crate) fn delete_shaker_entry(&mut self, key: &str) -> Result<bool, StoreError> {
        if let Some(database) = self.writable_metadata_database("shakers")? {
            match database.delete(key) {
                Ok(()) => {}
                Err(openrigcue_config::import::sqlitedict::DbError::MissingKey { .. }) => return Ok(false),
                Err(error) => return Err(self.metadata_error("select_one", error)),
            }
        } else if !self.config.metadata_table("shakers").contains(key) {
            return Ok(false);
        }
        let mut table = self.config.metadata_table("shakers");
        table.remove(key);
        self.update(|configuration| configuration.replace_native_shakers(table));
        Ok(true)
    }

    /// Closes the application's tables, each independently of the others. The
    /// table handles stay in place after a successful close;
    /// later access must not reopen the file.
    pub(crate) fn close_opened_files(&mut self) {
        self.close_profile_database();
        // The game notification table stays open here; the launcher closes it.
        self.close_metadata_tables(&["joystick", "shakers", "notifications"]);
    }

    /// The launcher additionally closes the game notification table. Its cache
    /// table is owned separately and closes after these tables.
    pub(crate) fn close_launcher_files(&mut self) {
        self.close_profile_database();
        self.close_metadata_tables(&["joystick", "shakers", "game_config_notifications", "notifications"]);
    }

    fn close_metadata_tables(&mut self, tables: &[&str]) {
        let path = self.dir.as_ref().filter(|_| self.native_files_initialized()).map(ConfigDir::metadata_file);
        let databases = self.metadata_db.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
        for &table in tables {
            if let Some(path) = &path {
                databases
                    .entry(table.to_owned())
                    .or_insert_with(|| Arc::new(AutocommitTable::closed_handle(path, table)));
            }
            let Some(database) = databases.get(table) else { continue };
            if let Err(error) = database.close() {
                tracing::debug!(%error,table,"Metadata database close raised");
            }
        }
    }
}

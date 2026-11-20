//! The raw tables of `metadata.db`, retained alongside their typed consumer views.

use std::collections::BTreeMap;
use std::path::Path;

use crate::import::migrate::LegacyState;
use crate::import::value::{Dict, LegacyValue};
use crate::import::{legacy_key_set, legacy_table, reconcile_detached, sqlitedict};
use crate::StoreError;

use super::Configuration;

pub const TABLES: [&str; 4] = ["shakers", "joystick", "game_config_notifications", "notifications"];

#[derive(Debug, Clone, Default)]
pub(crate) struct Metadata {
    original: BTreeMap<String, Dict>,
    projection: BTreeMap<String, Dict>,
}

impl Metadata {
    pub fn retain(&mut self, state: &LegacyState, configuration: &Configuration) {
        for (name, values) in [
            ("shakers", &state.shaker_layouts),
            ("joystick", &state.joysticks),
            ("game_config_notifications", &state.game_config_notifications),
            ("notifications", &state.notifications),
        ] {
            self.original.insert(name.to_owned(), values.clone());
            self.projection.insert(name.to_owned(), typed_table(configuration, name));
        }
    }

    pub fn table(&self, configuration: &Configuration, name: &str) -> Dict {
        let mut raw = LegacyValue::Dict(self.original.get(name).cloned().unwrap_or_default());
        let before = LegacyValue::Dict(self.projection.get(name).cloned().unwrap_or_default());
        // A stored row that is a shared object keeps what it holds: a typed edit of it is made in a copy.
        reconcile_detached(&mut raw, &before, &LegacyValue::Dict(typed_table(configuration, name)));
        let LegacyValue::Dict(table) = raw else { unreachable!("metadata table is a dictionary") };
        table
    }

    pub fn accept(&mut self, configuration: &Configuration, name: &str, committed: Dict) {
        self.original.insert(name.to_owned(), committed);
        self.projection.insert(name.to_owned(), typed_table(configuration, name));
    }
}

fn typed_table(configuration: &Configuration, name: &str) -> Dict {
    match name {
        "shakers" => legacy_table(&configuration.shaker_layouts.0),
        "joystick" => legacy_table(&configuration.joysticks.0),
        "game_config_notifications" => legacy_key_set(&configuration.dismissals.game_setup),
        "notifications" => legacy_key_set(&configuration.dismissals.notification_keys),
        _ => unreachable!("known metadata table"),
    }
}

/// Commits changed rows individually. SQL REPLACE moves each assigned row to the end;
/// untouched rows and unrelated metadata tables keep their order and values.
pub(super) fn commit_changes(path: &Path, name: &str, before: &Dict, after: &Dict) -> Result<Dict, StoreError> {
    let mut committed = before.clone();
    for key in before.str_keys() {
        if !after.contains(&key) {
            sqlitedict::delete_item(path, name, &key).map_err(|error| StoreError::parse(path, error))?;
            committed.remove(&key);
        }
    }
    for key in after.str_keys() {
        let value = after.get(&key).expect("table key");
        if !before.get(&key).is_some_and(|old| crate::import::unchanged_projection(old, value)) {
            sqlitedict::set_item(path, name, &key, value).map_err(|error| StoreError::parse(path, error))?;
            committed.remove(&key);
            committed.insert(key, value.clone());
        }
    }
    Ok(committed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::value::Key;
    use crate::settings::paths::ConfigDir;

    #[test]
    fn typed_saves_retain_raw_metadata_and_unrelated_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path());
        Configuration::first_run().save(&dir).unwrap();
        let raw_mapping = LegacyValue::Dict(
            [(
                Key::from("pitch"),
                LegacyValue::Dict(
                    [(Key::from("joystick"), LegacyValue::None), (Key::from("mode"), LegacyValue::None)]
                        .into_iter()
                        .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        );
        sqlitedict::set_item(&dir.metadata_file(), "joystick", "Unassigned", &raw_mapping).unwrap();
        sqlitedict::set_item(&dir.metadata_file(), "shakers", "Malformed", &LegacyValue::None).unwrap();
        sqlitedict::set_item(&dir.metadata_file(), "notifications", "flag", &false.into()).unwrap();
        sqlitedict::set_item(&dir.metadata_file(), "cache", "other", &LegacyValue::Bytes(vec![0, 255])).unwrap();
        let original: Vec<_> =
            TABLES.iter().map(|name| sqlitedict::read_table(&dir.metadata_file(), name).unwrap()).collect();
        let config = Configuration::load(&dir).unwrap();
        config.save(&dir).unwrap();
        for (name, expected) in TABLES.iter().zip(original) {
            assert_eq!(sqlitedict::read_table(&dir.metadata_file(), name).unwrap(), expected, "{name}");
        }
        assert_eq!(
            sqlitedict::read_table(&dir.metadata_file(), "cache").unwrap().get("other"),
            Some(&LegacyValue::Bytes(vec![0, 255]))
        );
    }

    #[test]
    fn autocommit_updates_only_changed_rows_and_acknowledges_row_order() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = ConfigDir::at(tmp.path());
        Configuration::first_run().save(&dir).unwrap();
        let before = Configuration::load(&dir).unwrap();
        let other = before.metadata_table("joystick");
        let mut changed = before.clone();
        let first = before.metadata_table("shakers").str_keys()[0].clone();
        let layout = changed.shaker_layouts.0.get_mut(&first).unwrap();
        layout.subwoofer.sfx.accel += 1.25;
        changed.commit_metadata(&dir, "shakers", &before).unwrap();
        let raw = sqlitedict::read_table(&dir.metadata_file(), "shakers").unwrap();
        assert_eq!(raw.str_keys().last(), Some(&first));
        assert_eq!(changed.metadata_table("shakers"), raw);
        assert_eq!(sqlitedict::read_table(&dir.metadata_file(), "joystick").unwrap(), other);
    }

    /// An accepted row can be an object its writer still holds. Reading the table with a typed edit outstanding
    /// shows the edit and leaves that object as it was, so the typed field set back to its earlier value reads
    /// as that value again.
    #[test]
    fn reading_a_table_writes_no_typed_edit_into_a_shared_row() {
        use openrigcue_model::native_value::NativeObject;
        let mut config = Configuration::first_run();
        let mut table = config.metadata_table("shakers");
        let first = table.str_keys()[0].clone();
        let stored = table.get(&first).unwrap().clone();
        let shared = LegacyValue::Object(NativeObject::new(stored.clone()));
        table.insert(first.as_str(), shared.clone());
        config.replace_native_shakers(table);
        let read = |config: &Configuration| config.metadata_table("shakers").get(&first).unwrap().clone();
        assert_eq!(read(&config), stored);
        let accel = config.shaker_layouts.0[&first].subwoofer.sfx.accel;
        config.shaker_layouts.0.get_mut(&first).unwrap().subwoofer.sfx.accel = accel + 1.25;
        assert_ne!(read(&config), stored, "the typed edit is read");
        assert!(shared == stored, "reading wrote into the shared row");
        config.shaker_layouts.0.get_mut(&first).unwrap().subwoofer.sfx.accel = accel;
        assert_eq!(read(&config), stored, "the typed field is back at its stored value");
    }
}

#[cfg(test)]
mod float_edit_tests {
    use super::*;

    #[test]
    fn metadata_commit_does_not_discard_an_explicit_zero_sign_edit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("metadata.db");
        let before: Dict = [("layout".into(), LegacyValue::List(vec![0.0.into()]))].into_iter().collect();
        let after: Dict = [("layout".into(), LegacyValue::List(vec![(-0.0).into()]))].into_iter().collect();
        sqlitedict::replace_table(&path, "shakers", &before).unwrap();
        let committed = commit_changes(&path, "shakers", &before, &after).unwrap();
        for values in [committed, sqlitedict::read_table(&path, "shakers").unwrap()] {
            let LegacyValue::List(values) = values.get("layout").unwrap() else { panic!("list") };
            let LegacyValue::Float(value) = values[0] else { panic!("float") };
            assert_eq!(value.to_bits(), (-0.0_f64).to_bits());
        }
    }
}

//! v123's temporary database copy and separate remove/rename stages.

use super::{as_dict, vehicles_of, Dict, Key, LegacyValue, Migrator, StepResult};
use crate::import::sqlitedict::{self, AutocommitTable, PendingTable};
use std::fs;
use std::path::{Path, PathBuf};

fn temporary(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("a");
    name.into()
}

fn clean_profile(game: &Key, value: &LegacyValue) -> Result<LegacyValue, String> {
    // This row is shallow-copied; child aliases stay live.
    let mut value = value.copy_builtin()?;
    let mut tune = as_dict(&mut value, &game.to_text())?;
    let mut vehicles = vehicles_of(&mut tune)?;
    let mut cursor = vehicles.cursor()?;
    while let Some(vehicle) = cursor.next(&vehicles)? {
        let mut tune = vehicles.child(&vehicle)?;
        if !matches!(tune.get("vehicles"),Some(nested)if nested.py_len()==Some(0)) {
            tune.insert("vehicles", LegacyValue::Dict(Dict::new()))?;
        }
    }
    Ok(value)
}

fn string_key(key: &Key) -> Result<&str, String> {
    match key {
        Key::Str(key) => Ok(key),
        _ => Err("sqlitedict keys must be strings".into()),
    }
}

pub(super) fn apply(m: &mut Migrator<'_, '_>) -> StepResult {
    run(m, |_| Ok(()))
}

fn run(m: &mut Migrator<'_, '_>, mut before: impl FnMut(&'static str) -> StepResult) -> StepResult {
    let Some(effects) = m.src.effects else {
        let mut profiles = Dict::new();
        for (game, value) in m.state.profiles.iter() {
            profiles.insert(game.clone(), clean_profile(game, value)?);
        }
        m.state.profiles = profiles;
        m.state.game_config_notifications = Dict::new();
        m.state.notifications = Dict::new();
        return Ok(());
    };
    let profile = effects.docs.join("profile.db");
    let metadata = effects.docs.join("metadata.db");
    let profile_temp = temporary(&profile);
    let metadata_temp = temporary(&metadata);
    let error = |error: sqlitedict::DbError| error.to_string();

    // These stores are opened before the migration chain runs.
    // Preserve the older-format inputs too when their DB file is absent.
    if !profile.exists() {
        sqlitedict::replace_table(&profile, "unnamed", &m.state.profiles).map_err(error)?;
    } else if !sqlitedict::table_names(&profile).map_err(error)?.iter().any(|name| name == "unnamed") {
        sqlitedict::ensure_table(&profile, "unnamed").map_err(error)?;
    }
    if !metadata.exists() {
        for (table, values) in [
            ("joystick", &m.state.joysticks),
            ("shakers", &m.state.shaker_layouts),
            ("game_config_notifications", &m.state.game_config_notifications),
            ("notifications", &m.state.notifications),
        ] {
            sqlitedict::replace_table(&metadata, table, values).map_err(error)?;
        }
    } else {
        let tables = sqlitedict::table_names(&metadata).map_err(error)?;
        for table in ["joystick", "shakers", "game_config_notifications"] {
            if !tables.iter().any(|name| name == table) {
                sqlitedict::ensure_table(&metadata, table).map_err(error)?;
            }
        }
    }

    // All three tables open before copying. Existing temporary rows/tables
    // remain: opening in create mode does not truncate an interrupted earlier copy.
    let profiles = PendingTable::open(&profile_temp, "unnamed").map_err(error)?;
    let joysticks = AutocommitTable::open(&metadata_temp, "joystick").map_err(error)?;
    let shakers = AutocommitTable::open(&metadata_temp, "shakers").map_err(error)?;
    for row in m.profile_rows()? {
        let (game, value) = row?;
        let value = clean_profile(&game, &value)?;
        profiles.assign(string_key(&game)?, Some(&value)).map_err(error)?;
        profiles.commit().map_err(error)?;
    }
    profiles.close().map_err(error)?;
    drop(profiles);
    for row in m.metadata_rows("joystick")? {
        let (key, value) = row?;
        joysticks.set(string_key(&key)?, &value).map_err(error)?;
    }
    joysticks.close().map_err(error)?;
    drop(joysticks);
    for row in m.metadata_rows("shakers")? {
        let (key, value) = row?;
        shakers.set(string_key(&key)?, &value).map_err(error)?;
    }
    shakers.close().map_err(error)?;
    drop(shakers);

    // Closing the non-autocommit source discards its pending transaction;
    // those logical values have already been committed to the temporary DB.
    if let Some(database) = &m.profile_db {
        database.close().map_err(error)?;
    }
    m.profile_db = None;
    for name in ["joystick", "shakers", "game_config_notifications"] {
        if let Some(database) = m.metadata_db.get(name) {
            database.close().map_err(error)?;
        }
    }
    m.metadata_db.clear();
    before("remove-profile")?;
    fs::remove_file(&profile).map_err(|error| error.to_string())?;
    before("rename-profile")?;
    fs::rename(&profile_temp, &profile).map_err(|error| error.to_string())?;
    before("remove-metadata")?;
    fs::remove_file(&metadata).map_err(|error| error.to_string())?;
    before("rename-metadata")?;
    fs::rename(&metadata_temp, &metadata).map_err(|error| error.to_string())?;

    before("reopen-profile")?;
    m.profile_db = Some(PendingTable::open(&profile, "unnamed").map_err(error)?);
    m.state.profiles =
        if m.snapshot_rebuild { sqlitedict::read_table(&profile, "unnamed").map_err(error)? } else { Dict::new() };
    before("reopen-joystick")?;
    m.metadata_db.insert("joystick".into(), AutocommitTable::open(&metadata, "joystick").map_err(error)?);
    m.state.joysticks =
        if m.snapshot_rebuild { sqlitedict::read_table(&metadata, "joystick").map_err(error)? } else { Dict::new() };
    before("reopen-shakers")?;
    m.metadata_db.insert("shakers".into(), AutocommitTable::open(&metadata, "shakers").map_err(error)?);
    m.state.shaker_layouts =
        if m.snapshot_rebuild { sqlitedict::read_table(&metadata, "shakers").map_err(error)? } else { Dict::new() };
    // There is no push delivery. Keep the existing local-dismissal
    // table and the game-configuration dismissal table.
    m.metadata_db.insert("notifications".into(), AutocommitTable::open(&metadata, "notifications").map_err(error)?);
    m.state.notifications = if m.snapshot_rebuild {
        sqlitedict::read_table(&metadata, "notifications").map_err(error)?
    } else {
        Dict::new()
    };
    m.metadata_db.insert(
        "game_config_notifications".into(),
        AutocommitTable::open(&metadata, "game_config_notifications").map_err(error)?,
    );
    m.state.game_config_notifications = if m.snapshot_rebuild {
        sqlitedict::read_table(&metadata, "game_config_notifications").map_err(error)?
    } else {
        Dict::new()
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::migrate::{LegacyLayouts, LegacyState, MigrationSources, Platform, StartEffects};
    use crate::import::shipped::ShippedData;

    #[test]
    fn runtime_rebuild_opens_all_dictionaries_before_decoding_leftover_temporary_rows() {
        for table in ["unnamed", "joystick", "shakers", "notifications", "game_config_notifications"] {
            let directory = tempfile::tempdir().unwrap();
            let profile = directory.path().join("profile.db");
            let metadata = directory.path().join("metadata.db");
            sqlitedict::set_item(
                &profile,
                "unnamed",
                "emulator",
                &LegacyValue::from(serde_json::json!({"vehicles":{}})),
            )
            .unwrap();
            for source in ["joystick", "shakers", "game_config_notifications"] {
                sqlitedict::ensure_table(&metadata, source).unwrap();
            }
            let target = temporary(if table == "unnamed" { &profile } else { &metadata });
            sqlitedict::ensure_table(&target, table).unwrap();
            rusqlite::Connection::open(&target)
                .unwrap()
                .execute(&format!("INSERT INTO {table} (key,value) VALUES ('Opaque',?)"), [vec![255_u8]])
                .unwrap();
            let mut state = LegacyState {
                config: [(
                    Key::from("APP"),
                    LegacyValue::from(serde_json::json!({
                        "config_file_version":"122", "first_time_installation":"no"
                    })),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            };
            let legacy = LegacyLayouts::Absent;
            let sources = MigrationSources {
                effects: Some(StartEffects { docs: directory.path(), patch: None }),
                ..ShippedData::get().sources(Platform::Linux, &legacy)
            };
            let mut migrator = Migrator::new(&mut state, &sources).unwrap();
            migrator.snapshot_rebuild = false;
            migrator.current = 123;
            let mut events = Vec::new();
            run(&mut migrator, |event| {
                events.push(event);
                Ok(())
            })
            .unwrap();
            assert_eq!(
                events,
                [
                    "remove-profile",
                    "rename-profile",
                    "remove-metadata",
                    "rename-metadata",
                    "reopen-profile",
                    "reopen-joystick",
                    "reopen-shakers"
                ]
            );
            let tables = sqlitedict::table_names(&metadata).unwrap();
            for expected in ["joystick", "shakers", "notifications", "game_config_notifications"] {
                assert!(tables.iter().any(|table| table == expected));
                assert!(migrator.metadata_db.contains_key(expected));
            }
            let mut rows = if table == "unnamed" {
                migrator.profile_rows().unwrap()
            } else {
                migrator.metadata_rows(table).unwrap()
            };
            assert!(rows.next().unwrap().is_err(), "{table}: decoding waits for the actual consumer");
            assert!(
                sqlitedict::read_item(if table == "unnamed" { &profile } else { &metadata }, table, "Opaque").is_err()
            );
        }
    }
}

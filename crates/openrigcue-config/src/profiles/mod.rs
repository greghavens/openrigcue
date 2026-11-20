//! The profile store (`profile.db`): one tune per game, optional per-vehicle tunes, how the
//! active tune is resolved for a running game/vehicle, when it is reloaded, and saving, resetting,
//! importing and exporting tunes.

mod reload;
mod srs_file;

use std::collections::BTreeMap;
use std::path::Path;

use openrigcue_model::python_compare::native_equal;

pub use reload::{ReloadTracker, Vehicle};
pub use srs_file::{
    export_all, export_all_rows, export_native_profile_from_lookup, export_profile, export_profile_from_lookup,
    read_profile_content, read_profile_file, ExportMetadata, FileGame, FileVehicle, ImportCheck, ProfileFile,
    ProfileFileError, SRS_PROFILE_VERSION,
};

use crate::catalog::{Catalog, GameInfo};
use crate::import::migrate::Platform;
use crate::import::value::{Dict, Key, LegacyValue};
use crate::import::{reconcile_detached, sqlitedict};
use crate::tunes::{FactoryTunes, Tune};
use crate::StoreError;

/// Catalog entries the Profile Manager never lists.
const HIDDEN_FROM_MANAGER: [&str; 1] = ["ccd"];

/// One game row of the Profile Manager and the vehicle rows under it.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagerEntry<'a> {
    pub game: &'a GameInfo,
    /// Vehicles with a stored tune, sorted by name.
    pub vehicles: Vec<openrigcue_model::native_value::PythonString>,
}

/// Which entry a resolved tune came from (`'G'` / `'V'`). The UI shows a game-level tune for a
/// known vehicle as "Profile not saved for this car, using game instead".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileLevel {
    Game,
    Vehicle,
}

/// Where a save goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveTarget {
    Game,
    Vehicle(String),
    /// An imported vehicle key which cannot pass through a Rust String.
    NativeVehicle(Key),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    /// The store has no entry for the game.
    #[error("no profile stored for game {0:?}")]
    UnknownGame(String),
    /// No factory tune exists for the game.
    #[error("no factory tune for game {0:?}")]
    NoFactoryTune(String),
    /// The game is stored but not in the catalog.
    #[error("game {0:?} is not in the catalog")]
    NotInCatalog(String),
    #[error("{0}")]
    Native(String),
}

/// The store key for a listener's game (`''`, `test` and `none`
/// mean the stand-by `emulator` profile) and the vehicle key (`0`/`''` mean none).
pub fn profile_key<'a>(game: &'a str, vehicle: &'a Vehicle) -> (&'a str, Option<&'a str>) {
    let game = if matches!(game, "" | "test" | "none") { "emulator" } else { game };
    let vehicle = match vehicle {
        Vehicle::Named(name) if !name.is_empty() => Some(name.as_str()),
        _ => None,
    };
    (game, vehicle)
}

/// The vehicle lookup, where a failed lookup yields nothing. Only integer/bool values index sequences;
/// numerically equal keys of different types (`1`, `1.0`, `true`) name the same dictionary entry.
fn native_vehicle_item(vehicles: &LegacyValue, vehicle: &LegacyValue) -> Option<LegacyValue> {
    vehicles.lookup_owned(vehicle).ok()
}

/// Game id → game tune (with its vehicle tunes).
#[derive(Debug, Clone, Default)]
pub struct ProfileStore {
    games: BTreeMap<String, Tune>,
    original: Dict,
    projection: Dict,
}

// Equality of the typed view. Raw-value equality is available through `native_table()`.
impl PartialEq for ProfileStore {
    fn eq(&self, other: &Self) -> bool {
        self.games == other.games
    }
}

impl ProfileStore {
    pub fn new(games: BTreeMap<String, Tune>) -> Self {
        let mut store = ProfileStore { games, ..Self::default() };
        store.retain_original(store.typed_table());
        store
    }

    pub fn get(&self, game: &str) -> Option<&Tune> {
        self.games.get(game)
    }

    pub fn games(&self) -> impl Iterator<Item = (&str, &Tune)> {
        self.games.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn insert(&mut self, game: &str, tune: Tune) {
        self.games.insert(game.to_owned(), tune);
    }

    fn typed_table(&self) -> Dict {
        self.games().map(|(game, tune)| (Key::from(game), LegacyValue::from(tune.to_json()))).collect()
    }

    pub(crate) fn retain_original(&mut self, original: Dict) {
        self.projection = self.typed_table();
        self.original = original;
    }

    /// The `profile.db` rows as stored, with only actual typed-view edits applied. Unchanged values,
    /// unknown fields, missing keys, explicit nulls and dictionary order are retained. A stored row that is
    /// a shared object keeps what it holds: a typed edit of it is made in a copy.
    pub fn native_table(&self) -> Dict {
        let mut value = LegacyValue::Dict(self.original.clone());
        let (before, after) = (LegacyValue::Dict(self.projection.clone()), LegacyValue::Dict(self.typed_table()));
        reconcile_detached(&mut value, &before, &after);
        let LegacyValue::Dict(table) = value else { unreachable!("profile table is a dictionary") };
        table
    }

    /// Assigns a raw row without the listener-to-standby key mapping (profile import uses its file's
    /// game key as written). Like an SQL REPLACE, assignment moves the row to the end of iteration.
    pub fn set_native_game(&mut self, game: &str, value: LegacyValue) {
        self.synchronize_native();
        self.accept_native_game(game, Some(value));
    }

    /// Folds any old typed-view edits into the canonical rows before a sequence of raw-row assignments.
    pub fn synchronize_native(&mut self) {
        self.retain_original(self.native_table());
    }

    /// Acknowledges one successful SQL assignment/deletion after synchronize_native. Only this row's
    /// typed view is rebuilt; bulk operations do not deserialize every other profile per assignment.
    pub fn accept_native_game(&mut self, game: &str, value: Option<LegacyValue>) {
        self.replace_native_game(game, value, true);
    }

    /// Refresh a row read from the database without giving that read REPLACE's
    /// row-order side effect.
    pub fn refresh_native_game(&mut self, game: &str, value: Option<LegacyValue>) {
        self.replace_native_game(game, value, false);
    }

    fn replace_native_game(&mut self, game: &str, value: Option<LegacyValue>, move_to_end: bool) {
        if move_to_end || value.is_none() {
            self.original.remove(game);
        }
        self.games.remove(game);
        self.projection.remove(game);
        let Some(value) = value else {
            return;
        };
        let table = [(Key::from(game), value.clone())].into_iter().collect();
        let mut warnings = Vec::new();
        let mut converted = crate::import::convert_profiles(&table, &mut warnings);
        if let Some(tune) = converted.games.remove(game) {
            self.projection.insert(game, LegacyValue::from(tune.to_json()));
            self.games.insert(game.to_owned(), tune);
        }
        self.original.insert(game, value);
    }

    fn game_mut(&mut self, game: &str) -> Result<&mut Tune, ProfileError> {
        self.games.get_mut(game).ok_or_else(|| ProfileError::UnknownGame(game.to_owned()))
    }

    /// The active tune: the vehicle tune when the game has vehicle
    /// tunes and one for this vehicle, otherwise a copy of the game tune with its vehicles removed.
    pub fn resolve(&self, game: &str, vehicle: &Vehicle) -> Result<(Tune, ProfileLevel), ProfileError> {
        let (game, vehicle) = profile_key(game, vehicle);
        let entry = self.games.get(game).ok_or_else(|| ProfileError::UnknownGame(game.to_owned()))?;
        if let Some(tune) = vehicle.and_then(|v| entry.vehicles.get(v)) {
            return Ok((tune.clone(), ProfileLevel::Vehicle));
        }
        let mut tune = entry.clone();
        tune.vehicles.clear();
        Ok((tune, ProfileLevel::Game))
    }

    /// The active tune from the canonical database values. A present vehicle entry is returned as is,
    /// including null or an empty dict; only the lookup failure falls back to the game's copy.
    pub fn resolve_native(&self, game: &str, vehicle: &Vehicle) -> Result<(LegacyValue, ProfileLevel), ProfileError> {
        let (game, vehicle) = profile_key(game, vehicle);
        let table = self.native_table();
        let entry = table.get(game).ok_or_else(|| ProfileError::UnknownGame(game.to_owned()))?;
        Self::resolve_native_row(game, vehicle, entry)
    }

    /// Resolve a row read from the live database connection. This keeps the
    /// canonical read independent of the typed profile snapshot.
    pub fn resolve_native_row(
        game: &str,
        vehicle: Option<&str>,
        entry: &LegacyValue,
    ) -> Result<(LegacyValue, ProfileLevel), ProfileError> {
        let vehicle = vehicle.map_or(LegacyValue::Int(0), |name| LegacyValue::Str(name.to_owned()));
        Self::resolve_native_identity_row(game, &vehicle, entry)
    }

    /// The store key and vehicle lookup for the raw value a listener reports.
    /// Null, zero-like values and empty text select the game; failed/unhashable lookups fall back.
    pub fn resolve_native_identity_row(
        _game: &str,
        vehicle: &LegacyValue,
        entry: &LegacyValue,
    ) -> Result<(LegacyValue, ProfileLevel), ProfileError> {
        if !matches!(vehicle.resolved(), LegacyValue::None)
            && !native_equal(vehicle, &LegacyValue::Int(0))
            && !native_equal(vehicle, &LegacyValue::Str(String::new()))
        {
            if let Ok(vehicles) = entry.item_owned("vehicles") {
                if vehicles.py_len().unwrap_or(0) != 0 {
                    if let Some(value) = native_vehicle_item(&vehicles, vehicle) {
                        return Ok((value, ProfileLevel::Vehicle));
                    }
                }
            }
        }
        let mut value = entry.copy_builtin().map_err(ProfileError::Native)?;
        value
            .assign_item("vehicles", LegacyValue::Dict(Dict::new()))
            .map_err(|detail| ProfileError::Native(format!("TypeError: {detail}")))?;
        Ok((value, ProfileLevel::Game))
    }

    /// Save using the active raw dictionary. A game save keeps the existing vehicles; a vehicle save
    /// assigns the entire supplied value, without converting unknown, missing or null fields.
    pub fn save_native(&mut self, game: &str, target: &SaveTarget, mut value: LegacyValue) -> Result<(), ProfileError> {
        let (game, _) = profile_key(game, &Vehicle::Unknown);
        let mut table = self.native_table();
        let current = table.get_mut(game).ok_or_else(|| ProfileError::UnknownGame(game.to_owned()))?;
        let target = if matches!(target, SaveTarget::Vehicle(vehicle) if vehicle.is_empty()) {
            &SaveTarget::Game
        } else {
            target
        };
        match target {
            SaveTarget::Game => {
                let vehicles =
                    current.item_owned("vehicles").map_err(|_| ProfileError::Native("KeyError: vehicles".into()))?;
                value
                    .assign_item("vehicles", vehicles)
                    .map_err(|detail| ProfileError::Native(format!("TypeError: {detail}")))?;
                *current = value;
            }
            SaveTarget::Vehicle(vehicle) => {
                current.assign_path(&["vehicles", vehicle.as_str()], value).map_err(ProfileError::Native)?;
            }
            SaveTarget::NativeVehicle(key) => {
                let mut vehicles = current.item_owned("vehicles").map_err(ProfileError::Native)?;
                vehicles.assign_key(key.to_value(), value).map_err(ProfileError::Native)?;
                current.assign_item("vehicles", vehicles).map_err(ProfileError::Native)?;
            }
        }
        let mut warnings = Vec::new();
        *self = crate::import::convert_profiles(&table, &mut warnings);
        Ok(())
    }

    /// Whether a vehicle tune is stored for this vehicle.
    pub fn has_vehicle_tune(&self, game: &str, vehicle: &Vehicle) -> bool {
        let (game, vehicle) = profile_key(game, vehicle);
        let table = self.native_table();
        vehicle
            .and_then(|vehicle| table.get(game)?.item_owned("vehicles").ok()?.item_owned(vehicle).ok())
            .and_then(|value| value.py_len())
            .is_some_and(|len| len > 0)
    }

    /// Save: a vehicle save replaces that vehicle's tune; a
    /// game save replaces the game tune but keeps the stored vehicle tunes.
    pub fn save(&mut self, game: &str, target: &SaveTarget, tune: Tune) -> Result<(), ProfileError> {
        if matches!(target, SaveTarget::NativeVehicle(_)) {
            return self.save_native(game, target, LegacyValue::from(tune.to_json()));
        }
        let vehicle = match target {
            SaveTarget::Game => Vehicle::Unknown,
            SaveTarget::Vehicle(name) => Vehicle::Named(name.clone()),
            SaveTarget::NativeVehicle(_) => unreachable!("handled above"),
        };
        let (game, vehicle) = profile_key(game, &vehicle);
        match vehicle {
            Some(v) => {
                self.game_mut(game)?.vehicles.insert(v.to_owned(), tune);
                Ok(())
            }
            None => self.replace_game_tune(game, tune),
        }
    }

    /// Replaces the tune stored under the key `game` (taken as is), keeping its vehicle tunes.
    fn replace_game_tune(&mut self, game: &str, mut tune: Tune) -> Result<(), ProfileError> {
        tune.vehicles = std::mem::take(&mut self.game_mut(game)?.vehicles);
        self.games.insert(game.to_owned(), tune);
        Ok(())
    }

    /// Reset a game: its entry is removed, then replaced by the factory tune, vehicle tunes
    /// included (the stored vehicle tunes are dropped). A game without a factory tune has its entry
    /// removed and then fails ([`ProfileError::NoFactoryTune`]).
    pub fn reset_game(&mut self, game: &str, factory: &FactoryTunes) -> Result<(), ProfileError> {
        let (game, _) = profile_key(game, &Vehicle::Unknown);
        self.games.remove(game);
        self.original.remove(game);
        self.projection.remove(game);
        let tune = factory.get(game).ok_or_else(|| ProfileError::NoFactoryTune(game.to_owned()))?;
        self.games.insert(game.to_owned(), tune.clone());
        if let Some(original) = crate::import::shipped::ShippedData::get().master_tunes.get(game) {
            self.original.insert(game, original.clone());
            self.projection.insert(game, LegacyValue::from(tune.to_json()));
        }
        Ok(())
    }

    /// Removes one vehicle tune if stored. `true` when one was removed (the caller then also forgets the
    /// current vehicle name).
    pub fn delete_vehicle(&mut self, game: &str, vehicle: &Vehicle) -> bool {
        let (game, vehicle) = profile_key(game, vehicle);
        let Some(vehicle) = vehicle else {
            return false;
        };
        let mut table = self.native_table();
        let Some(entry) = table.get_mut(game) else {
            return false;
        };
        let Ok(vehicles) = entry.item_owned("vehicles") else {
            return false;
        };
        if !vehicles.item_owned(vehicle).ok().and_then(|value| value.py_len()).is_some_and(|len| len > 0) {
            return false;
        }
        if entry.remove_path(&["vehicles", vehicle]).is_err() {
            return false;
        }
        self.set_native_game(game, entry.clone());
        true
    }

    /// The games the Profile Manager
    /// lists, sorted by lower-cased display name (catalog order on ties), each with its stored
    /// vehicles in sorted order. `ccd` is never listed; on any platform other than
    /// Windows only games whose platforms include Console are. Every catalog game must be stored:
    /// the first one missing (in listing order, before those rules apply) fails the whole listing.
    pub fn manager_entries<'a>(
        &'a self,
        catalog: &'a Catalog,
        platform: Platform,
    ) -> Result<Vec<ManagerEntry<'a>>, ProfileError> {
        let table = self.native_table();
        Self::manager_entries_from_lookup(catalog, platform, |key| Ok(table.get(key).cloned()))
    }

    /// The Profile Manager looks up catalog games in display order. Unrelated
    /// SQL rows are not decoded while building this listing.
    pub fn manager_entries_from_lookup<'a>(
        catalog: &'a Catalog,
        platform: Platform,
        mut lookup: impl FnMut(&str) -> Result<Option<LegacyValue>, ProfileError>,
    ) -> Result<Vec<ManagerEntry<'a>>, ProfileError> {
        let mut games: Vec<&GameInfo> = catalog.games().iter().collect();
        games.sort_by_key(|game| game.name.to_lowercase());
        let mut entries = Vec::new();
        for game in games {
            let tune = lookup(&game.id)?.ok_or_else(|| ProfileError::UnknownGame(game.id.clone()))?;
            let hidden = HIDDEN_FROM_MANAGER.contains(&game.id.as_str())
                || (platform != Platform::Windows && !game.supports_console());
            if !hidden {
                let vehicles = tune.item_owned("vehicles").ok().map(|value| value.resolved()).ok_or_else(|| {
                    ProfileError::Native(format!("game {:?}: vehicles is missing or is not a dictionary", game.id))
                })?;
                let vehicles = vehicles.as_dict().ok_or_else(|| {
                    ProfileError::Native(format!("game {:?}: vehicles is missing or is not a dictionary", game.id))
                })?;
                let mut vehicles: Vec<_> = vehicles.keys().cloned().collect();
                openrigcue_model::native_key_sort::sort_by_key(&mut vehicles, |key| key)
                    .map_err(ProfileError::Native)?;
                // The button key is game + '^' + vehicle;
                // arbitrary imported key kinds are never stringified for it.
                let vehicles = vehicles
                    .iter()
                    .map(|key| {
                        key.to_value().python_string().ok_or_else(|| {
                            ProfileError::Native(format!(
                                "TypeError: can only concatenate str (not \"{}\") to str",
                                key.type_name()
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                entries.push(ManagerEntry { game, vehicles });
            }
        }
        Ok(entries)
    }

    /// Reads the `profile.db` table, retaining its raw values and row order. A directory
    /// is accepted only for migration from per-game TOML files.
    pub fn load(path: &Path) -> Result<ProfileStore, StoreError> {
        if path.is_dir() && path.file_name().is_some_and(|name| name == "tunes") {
            return Self::load_old_toml(path);
        }
        if !path.exists() {
            return Ok(Self::default());
        }
        let table = sqlitedict::read_table(path, "unnamed").map_err(|error| StoreError::parse(path, error))?;
        let mut warnings = Vec::new();
        let store = crate::import::convert_profiles(&table, &mut warnings);
        for warning in warnings {
            tracing::debug!(%warning, "profile typed view");
        }
        Ok(store)
    }

    fn load_old_toml(dir: &Path) -> Result<ProfileStore, StoreError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProfileStore::default()),
            Err(e) => return Err(StoreError::Io { path: dir.to_owned(), source: e }),
        };
        let mut games = BTreeMap::new();
        for entry in entries {
            let path = entry.map_err(|e| StoreError::Io { path: dir.to_owned(), source: e })?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(game) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else { continue };
            if let Some(tune) = crate::settings::paths::read_toml::<Tune>(&path)? {
                games.insert(game, tune);
            }
        }
        Ok(ProfileStore::new(games))
    }

    /// Writes a whole migrated/imported profile table, preserving its raw dictionary values.
    pub fn save_all(&self, path: &Path) -> Result<(), StoreError> {
        create_parent(path)?;
        sqlitedict::replace_table(path, "unnamed", &self.native_table()).map_err(|error| StoreError::parse(path, error))
    }

    /// Writes and commits one game. REPLACE moves the game's row
    /// to the end of SQLite iteration. A failed write leaves this store's raw snapshot unchanged.
    pub fn save_game(&mut self, path: &Path, game: &str) -> Result<(), StoreError> {
        create_parent(path)?;
        match self.native_table().get(game).cloned() {
            Some(value) => {
                sqlitedict::set_item(path, "unnamed", game, &value).map_err(|error| StoreError::parse(path, error))?;
                self.original.remove(game);
                self.original.insert(game, value);
                if let Some(tune) = self.games.get(game) {
                    self.projection.insert(game, LegacyValue::from(tune.to_json()));
                }
            }
            None => {
                sqlitedict::delete_item(path, "unnamed", game).map_err(|error| StoreError::parse(path, error))?;
                self.original.remove(game);
                self.projection.remove(game);
            }
        }
        Ok(())
    }
}

fn create_parent(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| StoreError::io(parent, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

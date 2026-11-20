//! `.srs` profile files: a zip archive holding one JSON tune (four-space indent,
//! sorted keys) plus `game`, `vehicle`, `srs_version`, `motion_model` and `shaker_profile`.

use std::io::{Cursor, Read, Write};

use openrigcue_model::python_compare::native_equal;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{ProfileError, ProfileStore, SaveTarget, Vehicle};
use crate::catalog::Catalog;
use crate::import::de::{native_to_python_json, ConvertError};
use crate::import::value::{Dict, Key, LegacyValue, PythonString};

/// The `srs_version` written into exported profiles: the SRS release whose profile schema openrigcue
/// reads and writes.
pub const SRS_PROFILE_VERSION: &str = "2026.6.14.2";

const NOT_A_PROFILE: &str = "Import refused: this file is not a valid OpenRigCue profile";
const NOT_IN_USE: &str = "Import refused: the profile in this file does not correspond to the profile now in use.";
const UNREADABLE: &str = "Import refused: the content of the profile could not be read";

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProfileFileError {
    /// Not a zip, not UTF-8 JSON, or `game`/`vehicle`/`srs_version` missing.
    #[error("{NOT_A_PROFILE}")]
    NotAProfile,
    /// The profile is for another game/vehicle than the one in use.
    #[error("{NOT_IN_USE}")]
    NotInUse,
    /// The confirmed import failed (missing dicts, unknown game).
    #[error("{UNREADABLE}")]
    Unreadable,
    /// The profile lacks parts of the current tune schema (it is stored anyway).
    #[error("profile is incomplete: {0}")]
    Incomplete(ConvertError),
}

/// The `vehicle` value of a `.srs` file, classified by how it compares with the current vehicle
/// and with zero, and by whether it can be a dictionary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileVehicle {
    /// A value equal to 0 (`0`, `0.0`, `false`): a game-level profile.
    Game,
    /// A string: a profile for that vehicle.
    Named(String),
    /// A string containing code points excluded by Rust String.
    Unicode(PythonString),
    /// Any other number, `true` or `null`, held as its text form for text-keyed targets.
    /// Integers, booleans, non-NaN floats and null retain their typed storage key;
    /// unsupported fresh key kinds still use text. Offers compare the file's own value.
    Other(String),
    /// A list or object: the offer can match a raw current identity, but confirmation fails
    /// (unhashable dict key).
    Unhashable,
}

impl FileVehicle {
    fn classify(value: &LegacyValue) -> FileVehicle {
        match value {
            LegacyValue::Str(name) => FileVehicle::Named(name.clone()),
            LegacyValue::Unicode(name) => FileVehicle::Unicode(name.clone()),
            v if v.py_float() == Some(0.0) => FileVehicle::Game,
            v => v.py_str().map_or(FileVehicle::Unhashable, FileVehicle::Other),
        }
    }
}

/// The `game` value of a `.srs` file. Only a string can name a stored profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileGame {
    /// A string: the game id the profile is for.
    Id(String),
    /// `null`: equal to the forced game while no game is forced.
    Null,
    /// Any other value: equal to no running or forced game.
    Other,
}

impl FileGame {
    fn classify(value: &LegacyValue) -> FileGame {
        match value {
            LegacyValue::Str(id) => FileGame::Id(id.clone()),
            LegacyValue::None => FileGame::Null,
            _ => FileGame::Other,
        }
    }

    fn id(&self) -> Option<&str> {
        match self {
            FileGame::Id(id) => Some(id),
            _ => None,
        }
    }

    /// Whether this is the forced game.
    fn is_forced(&self, forced_game: Option<&str>) -> bool {
        match (self, forced_game) {
            (FileGame::Id(id), Some(forced)) => id == forced,
            (FileGame::Null, None) => true,
            _ => false,
        }
    }
}

/// A parsed `.srs` file.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileFile {
    pub game: FileGame,
    pub vehicle: FileVehicle,
    /// The tune dict with its metadata keys.
    content: Dict,
}

/// Reads a profile file up to the point where the import is offered: only the first member of
/// the archive is read. Its contents must be UTF-8 JSON with `game`, `vehicle` and `srs_version`.
pub fn read_profile_file(bytes: &[u8]) -> Result<ProfileFile, ProfileFileError> {
    ProfileFile::from_content(read_profile_content(bytes)?)
}

/// Archive decoding and JSON parsing only; required metadata validation and offer matching
/// are the caller's next steps.
pub fn read_profile_content(bytes: &[u8]) -> Result<LegacyValue, ProfileFileError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| ProfileFileError::NotAProfile)?;
    // Only the first member name is used. zip's name map retains first-name order and
    // the last member for duplicate names, so index 0 is the first name's last member.
    let mut member = archive.by_index(0).map_err(|_| ProfileFileError::NotAProfile)?;
    let mut content = Vec::new();
    member.read_to_end(&mut content).map_err(|_| ProfileFileError::NotAProfile)?;
    // UTF-8 decoding, then JSON parsing of the text (a BOM is refused,
    // `NaN`/`Infinity`/`-0` are accepted, a repeated key keeps its last value).
    let text = String::from_utf8(content).map_err(|_| ProfileFileError::NotAProfile)?;
    crate::import::json::parse(&text).map_err(|_| ProfileFileError::NotAProfile)
}

/// What the app is doing when a profile file is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCheck<'a> {
    /// The running game.
    pub game_running: &'a str,
    /// The current vehicle.
    pub vehicle: &'a Vehicle,
    /// The game forced by profile edit mode.
    pub forced_game: Option<&'a str>,
}

impl ProfileFile {
    pub fn from_content(content: LegacyValue) -> Result<Self, ProfileFileError> {
        let LegacyValue::Dict(content) = content else { return Err(ProfileFileError::NotAProfile) };
        if !content.contains("srs_version") {
            return Err(ProfileFileError::NotAProfile);
        }
        let game = FileGame::classify(content.get("game").ok_or(ProfileFileError::NotAProfile)?);
        let vehicle = FileVehicle::classify(content.get("vehicle").ok_or(ProfileFileError::NotAProfile)?);
        Ok(Self { game, vehicle, content })
    }

    /// The file's JSON metadata, before display or storage-key conversion.
    pub fn vehicle_value(&self) -> &LegacyValue {
        self.content.get("vehicle").expect("parsed vehicle metadata")
    }

    /// The import is offered when no game runs, or the profile is for the running (or edited)
    /// game and the current vehicle.
    pub fn check(&self, state: &ImportCheck<'_>) -> Result<(), ProfileFileError> {
        self.check_native(state.game_running, &state.vehicle.native_value(), state.forced_game)
    }

    /// Raw-value equality of the current vehicle and the file's, before any display or storage-key conversion.
    pub fn check_native(
        &self,
        game_running: &str,
        vehicle: &LegacyValue,
        forced_game: Option<&str>,
    ) -> Result<(), ProfileFileError> {
        let same_vehicle = native_equal(vehicle, self.vehicle_value());
        let allowed = game_running == "none"
            || (self.game.id() == Some(game_running) && same_vehicle)
            || (self.game.is_forced(forced_game) && same_vehicle);
        if allowed {
            Ok(())
        } else {
            Err(ProfileFileError::NotInUse)
        }
    }

    /// The confirmed import: drops the metadata keys, adds the effects older
    /// profiles lack (touchdown/turbulence shaker defaults, the front traction-loss family copied
    /// from the rear one), then stores the tune at game level (keeping the stored vehicle tunes) or
    /// as a vehicle tune, under the file's `game` as written (no stand-by mapping). The store is
    /// changed in memory; callers persist it when the user saves.
    pub fn import_into(&self, store: &mut ProfileStore) -> Result<SaveTarget, ProfileFileError> {
        let (game, target, value) = self.import_from_lookup(|game| Ok(store.native_table().get(game).cloned()))?;
        store.set_native_game(&game, value);
        Ok(target)
    }

    /// Prepare the imported row; missing effects are backfilled before the stored row is read.
    pub fn import_from_lookup(
        &self,
        lookup: impl FnOnce(&str) -> Result<Option<LegacyValue>, ProfileFileError>,
    ) -> Result<(String, SaveTarget, LegacyValue), ProfileFileError> {
        let mut content = self.content.clone();
        for key in ["game", "vehicle", "srs_version"] {
            content.remove(key);
        }
        // Removing `motion_model` and `shaker_profile` stops at the first one that is missing.
        if content.remove("motion_model").is_some() {
            content.remove("shaker_profile");
        }
        backfill_new_effects(&mut content).map_err(|_| ProfileFileError::Unreadable)?;
        let game = self.game.id().ok_or(ProfileFileError::Unreadable)?;
        let game_level = native_equal(self.vehicle_value(), &LegacyValue::Int(0));
        if !game_level {
            content.insert("vehicles", LegacyValue::Dict(Dict::new()));
        }
        let mut current = lookup(game)?.ok_or(ProfileFileError::Unreadable)?;
        let target = if game_level {
            let vehicles = current
                .item_owned("vehicles")
                .map_err(|_| ProfileFileError::Unreadable)?
                .copy_builtin()
                .map_err(|_| ProfileFileError::Unreadable)?;
            content.insert("vehicles", vehicles);
            current = content.into();
            SaveTarget::Game
        } else {
            current = current.copy_builtin().map_err(|_| ProfileFileError::Unreadable)?;
            let mut vehicles = current.item_owned("vehicles").map_err(|_| ProfileFileError::Unreadable)?;
            vehicles
                .assign_key(self.vehicle_value().clone(), content.into())
                .map_err(|_| ProfileFileError::Unreadable)?;
            current.assign_item("vehicles", vehicles).map_err(|_| ProfileFileError::Unreadable)?;
            match &self.vehicle {
                FileVehicle::Named(key) | FileVehicle::Other(key) => SaveTarget::Vehicle(key.clone()),
                FileVehicle::Unicode(key) => {
                    SaveTarget::NativeVehicle(Key::from_value(&LegacyValue::Unicode(key.clone())).expect("string key"))
                }
                FileVehicle::Unhashable => return Err(ProfileFileError::Unreadable),
                FileVehicle::Game => unreachable!("native numeric equality selected game above"),
            }
        };
        Ok((game.to_owned(), target, current))
    }
}

fn nested<'d>(dict: &'d mut Dict, key: &str) -> Result<&'d mut Dict, ()> {
    dict.get_mut(key).and_then(LegacyValue::as_dict_mut).ok_or(())
}

fn copy_key(dict: &mut Dict, from: &str, to: &str) -> Result<(), ()> {
    let value = dict.get(from).cloned().ok_or(())?;
    dict.insert(to, value);
    Ok(())
}

fn backfill_new_effects(content: &mut Dict) -> Result<(), ()> {
    if !nested(content, "shaker_effects")?.contains("touchdown") {
        for effect in ["turbulence", "touchdown"] {
            for (group, value) in [
                ("shaker_effects", LegacyValue::Int(10)),
                ("shaker_dsp_type", LegacyValue::Int(0)),
                ("shaker_cutoff", LegacyValue::Int(400)),
                ("shaker_resonance", LegacyValue::Int(10)),
                ("shaker_switch", LegacyValue::Bool(true)),
            ] {
                nested(content, group)?.insert(effect, value);
            }
        }
    }
    if !nested(content, "motion_effects")?.contains("traction_loss_front") {
        copy_key(content, "motion_main_actuators_traction_loss", "motion_main_actuators_traction_loss_front")?;
        copy_key(content, "motion_main_actuators_traction_loss", "min_traction_loss_front_activation")?;
        for group in ["motion_effects", "motion_switch", "max_telemetry_vars", "motion_effect_smoothing_factor"] {
            copy_key(nested(content, group)?, "traction_loss", "traction_loss_front")?;
        }
    }
    Ok(())
}

/// Hardware facts written into exports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportMetadata<'a> {
    /// `MOTIONBOX.model`.
    pub motion_model: &'a str,
    /// `SHAKEBOX.profile`.
    pub shaker_layout: &'a str,
    /// Names of the built-in shaker layouts.
    pub builtin_layouts: &'a [String],
}

impl ExportMetadata<'_> {
    /// Custom layouts export as `Custom`; the two retail layouts export under their SRS product names.
    fn exported_layout(&self) -> &str {
        match self.shaker_layout {
            "4 Speakers - FL,FR,RL,RR" => "SRS ShakeKit or SRS Shakebox Elite",
            "6 Speakers - 5.1" => "SRS Shakebox Elite + Sub + Center",
            name if self.builtin_layouts.iter().any(|b| b == name) => name,
            _ => "Custom",
        }
    }
}

fn export_json(
    tune: &LegacyValue,
    game: &str,
    vehicle: &Vehicle,
    meta: &ExportMetadata<'_>,
) -> Result<String, ProfileError> {
    export_native_json(tune, game, &vehicle.native_value(), meta)
}

fn export_native_json(
    tune: &LegacyValue,
    game: &str,
    vehicle: &LegacyValue,
    meta: &ExportMetadata<'_>,
) -> Result<String, ProfileError> {
    let mut config = tune.clone();
    for (key, value) in [
        ("vehicles", Dict::new().into()),
        ("game", game.into()),
        ("vehicle", vehicle.clone()),
        ("srs_version", SRS_PROFILE_VERSION.into()),
        ("motion_model", meta.motion_model.into()),
        ("shaker_profile", meta.exported_layout().into()),
    ] {
        config.assign_item(key, value).map_err(ProfileError::Native)?;
    }
    native_to_python_json(&config).map_err(|error| ProfileError::Native(error.message))
}

fn joined_name(prefix: &str, other: &PythonString) -> PythonString {
    PythonString::from_codepoints(prefix.chars().map(u32::from).chain(other.codepoints().iter().copied()).collect())
        .expect("Unicode points")
}
fn filename(value: &PythonString) -> Result<String, ProfileError> {
    value.scalar_string().ok_or_else(||{
        let start=value.codepoints().iter().position(|v|(0xd800..=0xdfff).contains(v)).expect("surrogate");
        let end=start+value.codepoints()[start..].iter().take_while(|v|(0xd800..=0xdfff).contains(*v)).count();
        ProfileError::Native(if end==start+1{format!("UnicodeEncodeError: 'utf-8' codec can't encode character '\\u{:04x}' in position {start}: surrogates not allowed",value.codepoints()[start])}else{format!("UnicodeEncodeError: 'utf-8' codec can't encode characters in position {start}-{}: surrogates not allowed",end-1)})
    })
}

/// Zip writer: stored (uncompressed) members, ZIP64 records only when a
/// member needs them.
fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
}

fn single_member_zip(name: &str, content: &[u8]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file(name, stored()).expect("writing to memory cannot fail");
    writer.write_all(content).expect("writing to memory cannot fail");
    writer.finish().expect("writing to memory cannot fail").into_inner()
}

/// Exports one profile: the vehicle tune when one is stored (named
/// `<game>_<vehicle>`), otherwise the game tune (named `<game>`). `location` is `None` for the
/// integer locations 0 and -1, which add nothing; a location string is appended as is.
/// Returns the file name (`.srs`) and the archive bytes. A stored game the catalog does not list
/// is refused: its catalog name is needed before the file is produced.
pub fn export_profile(
    store: &ProfileStore,
    catalog: &Catalog,
    game: &str,
    vehicle: &Vehicle,
    location: Option<&str>,
    meta: &ExportMetadata<'_>,
) -> Result<(String, Vec<u8>), ProfileError> {
    let table = store.native_table();
    let location = location.map(|value| LegacyValue::Str(value.to_owned()));
    export_profile_from_lookup(|key| Ok(table.get(key).cloned()), catalog, game, vehicle, location.as_ref(), meta)
}

/// Selected-row export falls back from the vehicle read to the game, including a
/// second game read after the first lookup fails or the vehicle is absent.
pub fn export_profile_from_lookup(
    lookup: impl FnMut(&str) -> Result<Option<LegacyValue>, ProfileError>,
    catalog: &Catalog,
    game: &str,
    vehicle: &Vehicle,
    location: Option<&LegacyValue>,
    meta: &ExportMetadata<'_>,
) -> Result<(String, Vec<u8>), ProfileError> {
    export_native_profile_from_lookup(lookup, catalog, game, &vehicle.native_value(), location, meta)
}

/// Raw button arguments survive lookup, filename fallback and exported metadata.
pub fn export_native_profile_from_lookup(
    mut lookup: impl FnMut(&str) -> Result<Option<LegacyValue>, ProfileError>,
    catalog: &Catalog,
    game: &str,
    vehicle: &LegacyValue,
    location: Option<&LegacyValue>,
    meta: &ExportMetadata<'_>,
) -> Result<(String, Vec<u8>), ProfileError> {
    let missing = || ProfileError::UnknownGame(game.to_owned());
    let (tune, mut name) = match vehicle.python_string() {
        Some(vehicle_name) => {
            let selected = lookup(game)
                .ok()
                .flatten()
                .and_then(|entry| entry.item_owned("vehicles").ok()?.lookup_owned(vehicle).ok());
            match selected {
                Some(value) => (value, joined_name(&format!("{game}_"), &vehicle_name)),
                None => (lookup(game)?.ok_or_else(missing)?, PythonString::from(game)),
            }
        }
        None if native_equal(vehicle, &LegacyValue::Int(0)) => {
            (lookup(game)?.ok_or_else(missing)?, PythonString::from(game))
        }
        _ => {
            // A failure of the lookup or of the `<game>_<vehicle>` name is handled the same way: a nontext
            // argument fails one of them, and the game is then read again.
            let _ = lookup(game);
            (lookup(game)?.ok_or_else(missing)?, PythonString::from(game))
        }
    };
    if let Some(location) = location {
        let ignored = native_equal(location, &LegacyValue::Int(0)) || native_equal(location, &LegacyValue::Int(-1));
        if !ignored {
            let Some(location) = location.python_string() else {
                return Err(ProfileError::Native(format!(
                    "TypeError: can only concatenate str (not \"{}\") to str",
                    location.type_name()
                )));
            };
            name = PythonString::from_codepoints(
                name.codepoints()
                    .iter()
                    .copied()
                    .chain([u32::from('_')])
                    .chain(location.codepoints().iter().copied())
                    .collect(),
            )
            .expect("Unicode name");
        }
    }
    let json = export_native_json(&tune, game, vehicle, meta)?;
    if !catalog.contains(game) {
        return Err(ProfileError::NotInCatalog(game.to_owned()));
    }
    let file_name = filename(
        &PythonString::from_codepoints(
            name.codepoints().iter().copied().chain(".srs".chars().map(u32::from)).collect(),
        )
        .expect("Unicode filename"),
    )?;
    let bytes = single_member_zip(&file_name, json.as_bytes());
    Ok((file_name, bytes))
}

/// Export All (`srs_export_all.zip`): every catalog
/// game and each of its vehicle tunes, each as a member that is itself a `.srs` archive.
pub fn export_all(store: &ProfileStore, catalog: &Catalog, meta: &ExportMetadata<'_>) -> Result<Vec<u8>, ProfileError> {
    let table = store.native_table();
    export_all_rows(table.iter().map(|(game, tune)| Ok((game.to_text(), tune.clone()))), catalog, meta)
}

/// Decode and export each database row in turn, one at a time.
pub fn export_all_rows(
    rows: impl IntoIterator<Item = Result<(String, LegacyValue), ProfileError>>,
    catalog: &Catalog,
    meta: &ExportMetadata<'_>,
) -> Result<Vec<u8>, ProfileError> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let mut add = |member: String, json: String| {
        let inner = single_member_zip(&member, json.as_bytes());
        writer.start_file(member, stored()).expect("writing to memory cannot fail");
        writer.write_all(&inner).expect("writing to memory cannot fail");
    };
    for row in rows {
        let (game, tune) = row?;
        if !catalog.contains(&game) {
            continue;
        }
        let config = tune.copy_builtin().map_err(ProfileError::Native)?;
        let vehicles = config
            .lookup_owned(&LegacyValue::Str("vehicles".into()))
            .map_err(ProfileError::Native)?
            .copy_builtin()
            .map_err(ProfileError::Native)?;
        add(format!("{game}.srs"), export_json(&config, &game, &Vehicle::Unknown, meta)?);
        let vehicles = vehicles.resolved();
        let vehicles = vehicles.as_dict().ok_or_else(|| {
            ProfileError::Native(format!("AttributeError: '{}' object has no attribute 'items'", vehicles.type_name()))
        })?;
        for (vehicle, vehicle_tune) in vehicles.iter() {
            let mut config = vehicle_tune.clone();
            config
                .assign_item("vehicles", Dict::new().into())
                .map_err(|error| ProfileError::Native(format!("TypeError: {error}")))?;
            // Assignment precedes the filename argument; JSON serialization follows
            // it. Unlike selected export, Export All does not recover from a failure of this name.
            let kind = vehicle.type_name();
            let Some(vehicle_name) = vehicle.to_value().python_string() else {
                return Err(ProfileError::Native(format!(
                    "TypeError: can only concatenate str (not \"{kind}\") to str"
                )));
            };
            let name = PythonString::from_codepoints(
                format!("{game}_")
                    .chars()
                    .map(u32::from)
                    .chain(vehicle_name.codepoints().iter().copied())
                    .chain(".srs".chars().map(u32::from))
                    .collect(),
            )
            .expect("Unicode filename");
            let json = export_native_json(&config, &game, &vehicle.to_value(), meta)?;
            add(filename(&name)?, json);
        }
    }
    Ok(writer.finish().expect("writing to memory cannot fail").into_inner())
}

#[cfg(test)]
mod lookup_tests {
    use super::*;

    #[test]
    fn native_export_retains_key_kinds_zero_and_caught_filename_fallback() {
        let meta = ExportMetadata { motion_model: "Fixture", shaker_layout: "Fixture", builtin_layouts: &[] };
        let mut game = Dict::new();
        game.insert("marker", 99.into());
        let mut vehicles = Dict::new();
        for key in [Key::Int(2), Key::from("2"), Key::from("")] {
            let mut car = Dict::new();
            car.insert("marker", 43.into());
            vehicles.insert(key, car.into());
        }
        game.insert("vehicles", vehicles.into());
        let game = LegacyValue::Dict(game);
        for (vehicle, expected_reads, expected_name, expected_marker) in [
            (LegacyValue::Int(2), 2, "ac.srs", 99),
            (LegacyValue::Str("2".into()), 1, "ac_2.srs", 43),
            (LegacyValue::Str("".into()), 1, "ac_.srs", 43),
            (LegacyValue::Int(0), 1, "ac.srs", 99),
            (LegacyValue::None, 2, "ac.srs", 99),
        ] {
            let mut reads = 0;
            let (name, bytes) = export_native_profile_from_lookup(
                |_| {
                    reads += 1;
                    Ok(Some(game.clone()))
                },
                Catalog::builtin(),
                "ac",
                &vehicle,
                None,
                &meta,
            )
            .unwrap();
            assert_eq!(reads, expected_reads);
            assert_eq!(name, expected_name);
            let file = read_profile_file(&bytes).unwrap();
            assert_eq!(file.vehicle_value(), &vehicle);
            assert_eq!(file.content.get("marker"), Some(&LegacyValue::Int(expected_marker)));
        }
        let mut reads = 0;
        let (_, bytes) = export_native_profile_from_lookup(
            |_| {
                reads += 1;
                if reads == 1 {
                    Err(ProfileError::Native("first read failed".into()))
                } else {
                    Ok(Some(game.clone()))
                }
            },
            Catalog::builtin(),
            "ac",
            &LegacyValue::Int(2),
            None,
            &meta,
        )
        .unwrap();
        assert_eq!(reads, 2);
        assert_eq!(read_profile_file(&bytes).unwrap().vehicle_value(), &LegacyValue::Int(2));
    }

    #[test]
    fn export_all_rejects_nontext_names_before_json_but_after_vehicle_assignment() {
        let meta = ExportMetadata { motion_model: "Fixture", shaker_layout: "Fixture", builtin_layouts: &[] };
        for (key, kind) in [
            (Key::Int(2), "int"),
            (Key::Bool(true), "bool"),
            (Key::float(1.5).unwrap(), "float"),
            (Key::None, "NoneType"),
        ] {
            let concatenate = format!("can only concatenate str (not \"{kind}\") to str");
            for (car, expected) in [
                (LegacyValue::Dict(Dict::new()), concatenate.clone()),
                (LegacyValue::None, "'NoneType' object does not support item assignment".into()),
                (
                    {
                        let mut car = Dict::new();
                        car.insert(Key::Int(1), 1.into());
                        car.into()
                    },
                    concatenate,
                ),
            ] {
                let mut vehicles = Dict::new();
                vehicles.insert(key.clone(), car);
                let mut row = Dict::new();
                row.insert("vehicles", vehicles.into());
                let mut read_count = 0;
                let rows = [Ok(("ac".into(), row.into())), Ok(("acc".into(), Dict::new().into()))]
                    .into_iter()
                    .inspect(|_| read_count += 1);
                let error = export_all_rows(rows, Catalog::builtin(), &meta).unwrap_err();
                assert_eq!(error.to_string(), format!("TypeError: {expected}"));
                assert_eq!(read_count, 1, "the next SQL row is not fetched after the vehicle error");
            }
        }
    }

    #[test]
    fn native_import_keeps_surrogate_keys_and_shallow_row_aliases() {
        use openrigcue_model::native_value::NativeObject;
        let vehicle = LegacyValue::Unicode(PythonString::from_codepoints(vec![65, 0xd800]).unwrap());
        let content = LegacyValue::Dict(
            [
                (Key::from("game"), "emulator".into()),
                (Key::from("vehicle"), vehicle.clone()),
                (Key::from("srs_version"), "fixture".into()),
                (Key::from("shaker_effects"), Dict::from_iter([(Key::from("touchdown"), 10.into())]).into()),
                (Key::from("motion_effects"), Dict::from_iter([(Key::from("traction_loss_front"), 20.into())]).into()),
            ]
            .into_iter()
            .collect(),
        );
        let file = ProfileFile::from_content(content).unwrap();
        let mut row = LegacyValue::Object(NativeObject::new(
            Dict::from_iter([(Key::from("vehicles"), Dict::new().into())]).into(),
        ));
        row.assign_item("self", row.clone()).unwrap();
        let vehicles = row.item_owned("vehicles").unwrap();
        let (_, target, result) = file.import_from_lookup(|_| Ok(Some(row.clone()))).unwrap();
        assert!(matches!(target, SaveTarget::NativeVehicle(_)));
        assert!(!result.same_object(&row), "source copies the outer game dictionary");
        assert!(result.item_owned("self").unwrap().same_object(&row));
        assert!(result.item_owned("vehicles").unwrap().same_object(&vehicles));
        assert!(vehicles.lookup_owned(&vehicle).is_ok(), "the shallow copy retains the actual vehicle container");
    }
}

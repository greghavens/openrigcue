use std::io::{Cursor, Read, Write};

use super::*;
use crate::catalog::Catalog;
use crate::import::migrate::Platform;
use crate::import::value::LegacyValue;
use crate::tunes::FactoryTunes;

fn factory(game: &str) -> Tune {
    FactoryTunes::builtin().get(game).unwrap().clone()
}

fn store() -> ProfileStore {
    let mut s = ProfileStore::default();
    for game in ["ac", "api", "emulator", "condor3"] {
        s.insert(game, factory(game));
    }
    s
}

fn car(name: &str) -> Vehicle {
    Vehicle::named(name)
}

/// Store keys for a listener's game and vehicle.
#[test]
fn profile_keys() {
    assert_eq!(profile_key("", &Vehicle::Unknown), ("emulator", None));
    assert_eq!(profile_key("test", &car("x")), ("emulator", Some("x")));
    assert_eq!(profile_key("none", &Vehicle::Unknown), ("emulator", None));
    assert_eq!(profile_key("ac", &car("")), ("ac", None));
    assert_eq!(profile_key("ac", &car("bmw")), ("ac", Some("bmw")));
}

/// Resolving the active tune: vehicle tune when stored, else the game tune without vehicles.
#[test]
fn resolution_rules() {
    let s = store();
    let (tune, level) = s.resolve("api", &car("Torque Drift")).unwrap();
    assert_eq!(level, ProfileLevel::Vehicle);
    assert_eq!(&tune, &factory("api").vehicles["Torque Drift"]);
    let (tune, level) = s.resolve("api", &car("Other")).unwrap();
    assert_eq!(level, ProfileLevel::Game);
    assert!(tune.vehicles.is_empty());
    assert_eq!(tune.gear_shift_perc, factory("api").gear_shift_perc);
    let (_, level) = s.resolve("ac", &car("bmw")).unwrap();
    assert_eq!(level, ProfileLevel::Game, "no vehicle tunes stored");
    let (tune, _) = s.resolve("none", &Vehicle::Unknown).unwrap();
    assert_eq!(tune.motion_smoothing_factor, factory("emulator").motion_smoothing_factor);
    assert_eq!(s.resolve("iracing", &Vehicle::Unknown), Err(ProfileError::UnknownGame("iracing".into())));
    assert!(s.has_vehicle_tune("api", &car("Flashout 3")));
    assert!(!s.has_vehicle_tune("api", &Vehicle::Unknown));
}

#[test]
fn native_resolution_returns_present_empty_and_none_vehicle_values() {
    let mut s = store();
    s.save_native("ac", &SaveTarget::Vehicle("None".into()), LegacyValue::None).unwrap();
    s.save_native("ac", &SaveTarget::Vehicle("Empty".into()), Dict::new().into()).unwrap();
    assert_eq!(s.resolve_native("ac", &car("None")).unwrap(), (LegacyValue::None, ProfileLevel::Vehicle));
    assert_eq!(s.resolve_native("ac", &car("Empty")).unwrap(), (Dict::new().into(), ProfileLevel::Vehicle));
    let (fallback, level) = s.resolve_native("ac", &car("missing")).unwrap();
    assert_eq!(level, ProfileLevel::Game);
    assert!(fallback.as_dict().unwrap().get("vehicles").unwrap().as_dict().unwrap().is_empty());
    assert!(!s.has_vehicle_tune("ac", &car("None")));
    assert!(!s.has_vehicle_tune("ac", &car("Empty")));
    assert!(!s.delete_vehicle("ac", &car("Empty")), "reset_vehicle_config leaves empty profiles alone");
    let malformed = LegacyValue::Dict([(Key::from("unknown"), LegacyValue::None)].into_iter().collect());
    s.save_native("ac", &SaveTarget::Vehicle("Malformed".into()), malformed).unwrap();
    assert!(s.has_vehicle_tune("ac", &car("Malformed")));
    assert!(s.delete_vehicle("ac", &car("Malformed")));
}

#[test]
fn native_vehicle_lookup_does_not_stringify_keys_and_catches_failed_subscripts() {
    let mut vehicles = Dict::new();
    vehicles.insert(Key::Int(1), LegacyValue::None);
    vehicles.insert("1", LegacyValue::Str("text tune".into()));
    vehicles.insert("None", LegacyValue::Str("named None tune".into()));
    let mut row = Dict::new();
    row.insert("vehicles", vehicles.into());
    row.insert("marker", LegacyValue::Int(43));
    let lookup = |row: &Dict, vehicle: &LegacyValue| {
        ProfileStore::resolve_native_identity_row("ac", vehicle, &LegacyValue::Dict(row.clone())).unwrap()
    };
    for vehicle in [LegacyValue::Int(1), LegacyValue::Bool(true), LegacyValue::Float(1.0)] {
        assert_eq!(lookup(&row, &vehicle), (LegacyValue::None, ProfileLevel::Vehicle));
    }
    assert_eq!(lookup(&row, &LegacyValue::Str("1".into())), ("text tune".into(), ProfileLevel::Vehicle));
    assert_eq!(lookup(&row, &LegacyValue::Str("None".into())), ("named None tune".into(), ProfileLevel::Vehicle));
    for vehicle in [
        LegacyValue::None,
        LegacyValue::Bool(false),
        LegacyValue::Float(-0.0),
        LegacyValue::Str(String::new()),
        LegacyValue::List(vec![LegacyValue::Int(1)]),
        LegacyValue::Float(f64::NAN),
        LegacyValue::Float(f64::INFINITY),
        LegacyValue::Int(9),
    ] {
        let (value, level) = lookup(&row, &vehicle);
        assert_eq!(level, ProfileLevel::Game, "{vehicle:?}");
        assert_eq!(value.as_dict().unwrap().get("marker"), Some(&LegacyValue::Int(43)));
        assert_eq!(value.as_dict().unwrap().get("vehicles"), Some(&LegacyValue::Dict(Dict::new())));
    }
    for values in [
        LegacyValue::List(vec!["first".into(), "last".into()]),
        LegacyValue::Tuple(vec!["first".into(), "last".into()]),
    ] {
        row.insert("vehicles", values);
        assert_eq!(lookup(&row, &LegacyValue::Int(-1)), ("last".into(), ProfileLevel::Vehicle));
        assert_eq!(lookup(&row, &LegacyValue::Bool(true)), ("last".into(), ProfileLevel::Vehicle));
        assert_eq!(lookup(&row, &LegacyValue::Float(1.0)).1, ProfileLevel::Game);
    }
    row.insert("vehicles", LegacyValue::Str("a\u{00e9}".into()));
    assert_eq!(lookup(&row, &LegacyValue::Int(-1)), ("\u{00e9}".into(), ProfileLevel::Vehicle));
    row.insert("vehicles", LegacyValue::Bytes(vec![3, 255]));
    assert_eq!(lookup(&row, &LegacyValue::Int(1)), (LegacyValue::Int(255), ProfileLevel::Vehicle));
    row.insert("vehicles", LegacyValue::Int(4));
    assert_eq!(lookup(&row, &LegacyValue::Int(1)).1, ProfileLevel::Game);
}

#[test]
fn native_save_keeps_raw_game_fields_and_vehicle_values() {
    let mut s = store();
    s.save_native("ac", &SaveTarget::Vehicle("None".into()), LegacyValue::None).unwrap();
    let mut raw = s.resolve_native("ac", &Vehicle::Unknown).unwrap().0;
    let fields = raw.as_dict_mut().unwrap();
    fields.remove("motion_reaction_speed");
    fields.insert("haptic_effects", Dict::new().into());
    fields.insert("custom", LegacyValue::Tuple(vec![LegacyValue::None, 1.into()]));
    s.save_native("ac", &SaveTarget::Vehicle(String::new()), raw.clone()).unwrap();
    assert_eq!(s.resolve_native("ac", &Vehicle::Unknown).unwrap().0, raw);
    assert_eq!(s.resolve_native("ac", &car("None")).unwrap().0, LegacyValue::None);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("profile.db");
    s.save_game(&path, "ac").unwrap();
    let restored = ProfileStore::load(&path).unwrap();
    assert_eq!(restored.native_table(), sqlitedict::read_table(&path, "unnamed").unwrap());
    assert_eq!(restored.resolve_native("ac", &Vehicle::Unknown).unwrap().0, raw);
}

/// Save, game reset and vehicle reset.
#[test]
fn save_reset_delete() {
    let mut s = store();
    let mut edited = s.resolve("api", &Vehicle::Unknown).unwrap().0;
    edited.motion_max_power = 42.0;
    s.save("api", &SaveTarget::Game, edited.clone()).unwrap();
    assert_eq!(s.get("api").unwrap().motion_max_power, 42.0);
    assert_eq!(s.get("api").unwrap().vehicles.len(), 4, "game save keeps vehicle tunes");

    s.save("ac", &SaveTarget::Vehicle("bmw".into()), edited.clone()).unwrap();
    assert_eq!(s.resolve("ac", &car("bmw")).unwrap().1, ProfileLevel::Vehicle);
    s.save("ac", &SaveTarget::Vehicle(String::new()), edited.clone()).unwrap();
    assert_eq!(s.get("ac").unwrap().motion_max_power, 42.0, "'' vehicle saves at game level");
    assert_eq!(s.get("ac").unwrap().vehicles.len(), 1);

    s.delete_vehicle("ac", &car("bmw"));
    assert!(s.get("ac").unwrap().vehicles.is_empty());
    s.reset_game("api", FactoryTunes::builtin()).unwrap();
    assert_eq!(s.get("api").unwrap(), &factory("api"));
    assert_eq!(s.save("iracing", &SaveTarget::Game, edited), Err(ProfileError::UnknownGame("iracing".into())));
}

/// Resetting a game without a factory tune removes the entry and then fails on the missing
/// factory tune; saving that game removes its database row.
#[test]
fn reset_without_factory_tune_removes_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("profile.db");
    let mut s = store();
    let tune = s.get("api").unwrap().clone();
    s.insert("not_a_factory_game", tune);
    s.save_all(&database).unwrap();
    assert_eq!(
        s.reset_game("not_a_factory_game", FactoryTunes::builtin()),
        Err(ProfileError::NoFactoryTune("not_a_factory_game".into()))
    );
    assert!(s.get("not_a_factory_game").is_none());
    s.save_game(&database, "not_a_factory_game").unwrap();
    let raw = crate::import::sqlitedict::read_table(&database, "unnamed").unwrap();
    assert!(!raw.contains("not_a_factory_game"));
    assert!(raw.contains("api"));
}

#[test]
fn store_files_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let s = store();
    let database = dir.path().join("profile.db");
    s.save_all(&database).unwrap();
    assert!(database.is_file());
    assert_eq!(ProfileStore::load(&database).unwrap(), s);
    assert_eq!(ProfileStore::load(&dir.path().join("missing")).unwrap(), ProfileStore::default());
}

#[test]
fn native_saves_preserve_unedited_types_missing_null_unknown_fields_and_vehicle_order() {
    use crate::import::{shipped::ShippedData, sqlitedict};
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("profile.db");
    let mut raw = ShippedData::get().master_tunes.get("ac").unwrap().clone();
    let game = raw.as_dict_mut().unwrap();
    game.remove("intellitune_motion_preference");
    game.insert("unknown", LegacyValue::List(vec![LegacyValue::None, 7.into(), true.into()]));
    game.insert(
        "routines",
        LegacyValue::Dict(
            [(
                Key::from("app"),
                LegacyValue::Dict(
                    [(Key::from("enabled"), true.into()), (Key::from("profile"), LegacyValue::None)]
                        .into_iter()
                        .collect(),
                ),
            )]
            .into_iter()
            .collect(),
        ),
    );
    let mut vehicle = raw.clone();
    vehicle.as_dict_mut().unwrap().insert("intellitune_motion_preference", LegacyValue::None);
    let vehicles: Dict = [(Key::from("zeta"), vehicle.clone()), (Key::from("alpha"), vehicle)].into_iter().collect();
    raw.as_dict_mut().unwrap().insert("vehicles", LegacyValue::Dict(vehicles));
    sqlitedict::set_item(&database, "unnamed", "ac", &raw).unwrap();
    let mut store = ProfileStore::load(&database).unwrap();
    assert_eq!(store.native_table().get("ac"), Some(&raw));
    let (mut tune, _) = store.resolve("ac", &Vehicle::Unknown).unwrap();
    tune.motion_max_power = 37.25;
    store.save("ac", &SaveTarget::Game, tune).unwrap();
    store.save_game(&database, "ac").unwrap();
    let mut expected = raw;
    expected.as_dict_mut().unwrap().insert("motion_max_power", LegacyValue::Float(37.25));
    let written = sqlitedict::read_table(&database, "unnamed").unwrap();
    assert_eq!(written.get("ac"), Some(&expected));
    assert_eq!(ProfileStore::load(&database).unwrap().native_table(), written);
}

#[test]
fn invalid_profile_values_are_retained_on_disk_without_a_typed_edit() {
    use crate::import::{shipped::ShippedData, sqlitedict};
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("profile.db");
    let mut raw = ShippedData::get().master_tunes.get("ac").unwrap().clone();
    raw.as_dict_mut().unwrap().insert("motion_max_power", "invalid".into());
    sqlitedict::set_item(&database, "unnamed", "ac", &raw).unwrap();
    sqlitedict::set_item(&database, "unnamed", "unconvertible", &LegacyValue::None).unwrap();
    let before = sqlitedict::read_table(&database, "unnamed").unwrap();
    let store = ProfileStore::load(&database).unwrap();
    store.save_all(&database).unwrap();
    assert_eq!(sqlitedict::read_table(&database, "unnamed").unwrap(), before);
}

#[test]
fn per_game_commits_keep_sqlitedict_iteration_order_in_memory_and_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("profile.db");
    let mut store = store();
    let original = store.native_table();
    store.retain_original(original);
    store.save_all(&database).unwrap();
    store.save_game(&database, "ac").unwrap();
    let on_disk = crate::import::sqlitedict::read_table(&database, "unnamed").unwrap();
    assert_eq!(on_disk.str_keys().last().map(String::as_str), Some("ac"));
    assert_eq!(store.native_table(), on_disk);
}

#[test]
fn old_rust_toml_profiles_can_be_read_and_saved_as_native_rows() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("tunes");
    std::fs::create_dir(&old).unwrap();
    let tune = factory("ac");
    std::fs::write(old.join("ac.toml"), toml::to_string(&tune).unwrap()).unwrap();
    let store = ProfileStore::load(&old).unwrap();
    let database = dir.path().join("profile.db");
    store.save_all(&database).unwrap();
    assert_eq!(ProfileStore::load(&database).unwrap().get("ac"), Some(&tune));
    assert!(old.join("ac.toml").is_file(), "the old files remain as a backup");
}

fn meta() -> (Vec<String>, &'static str) {
    (crate::catalog::registry::Registry::builtin().builtin_shaker_layouts.clone(), "P3")
}

fn unzip_single(bytes: &[u8]) -> (String, Vec<u8>) {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    assert_eq!(archive.len(), 1);
    let mut member = archive.by_index(0).unwrap();
    assert_eq!(member.compression(), zip::CompressionMethod::Stored);
    let mut content = Vec::new();
    member.read_to_end(&mut content).unwrap();
    (member.name().to_owned(), content)
}

/// Single-profile export and its hardware metadata.
#[test]
fn export_profile_format() {
    let (layouts, model) = meta();
    let m =
        ExportMetadata { motion_model: model, shaker_layout: "4 Speakers - FL,FR,RL,RR", builtin_layouts: &layouts };
    let (name, bytes) = export_profile(&store(), Catalog::builtin(), "api", &car("Torque Drift"), None, &m).unwrap();
    assert_eq!(name, "api_Torque Drift.srs");
    let (member, content) = unzip_single(&bytes);
    assert_eq!(member, "api_Torque Drift.srs");
    let text = String::from_utf8(content).unwrap();
    assert!(text.starts_with("{\n    \"belt_heave_enabled\": true,\n"), "sorted keys, indent 4: {}", &text[..60]);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["game"], "api");
    assert_eq!(json["vehicle"], "Torque Drift");
    assert_eq!(json["srs_version"], SRS_PROFILE_VERSION);
    assert_eq!(json["motion_model"], "P3");
    assert_eq!(json["shaker_profile"], "SRS ShakeKit or SRS Shakebox Elite");
    assert_eq!(json["vehicles"], serde_json::json!({}));

    let custom = ExportMetadata { shaker_layout: "My rig", ..m.clone() };
    let (name, bytes) =
        export_profile(&store(), Catalog::builtin(), "api", &car("Missing car"), Some("Nordschleife"), &custom)
            .unwrap();
    assert_eq!(name, "api_Nordschleife.srs", "falls back to the game tune");
    let json: serde_json::Value = serde_json::from_slice(&unzip_single(&bytes).1).unwrap();
    assert_eq!(json["vehicle"], "Missing car");
    assert_eq!(json["shaker_profile"], "Custom");
    let (name, _) = export_profile(&store(), Catalog::builtin(), "ac", &Vehicle::Unknown, None, &m).unwrap();
    assert_eq!(name, "ac.srs", "integer location 0 / -1");
    let (name, _) = export_profile(&store(), Catalog::builtin(), "ac", &Vehicle::Unknown, Some("0"), &m).unwrap();
    assert_eq!(name, "ac_0.srs", "only the integers 0 and -1 add nothing; str(location) is appended");
    let (name, _) = export_profile(&store(), Catalog::builtin(), "ac", &Vehicle::Unknown, Some("-1"), &m).unwrap();
    assert_eq!(name, "ac_-1.srs");
}

/// The zip writer emits ZIP64 records only when needed,
/// so a small member has a local header without extra field.
#[test]
fn export_archives_have_no_zip64_records() {
    let (layouts, model) = meta();
    let m = ExportMetadata { motion_model: model, shaker_layout: "SRS UShake6", builtin_layouts: &layouts };
    let (_, bytes) = export_profile(&store(), Catalog::builtin(), "ac", &Vehicle::Unknown, None, &m).unwrap();
    assert_eq!(&bytes[..4], b"PK\x03\x04");
    assert_eq!(u16::from_le_bytes([bytes[28], bytes[29]]), 0, "local header extra field length");
    let all = export_all(&store(), Catalog::builtin(), &m).unwrap();
    assert_eq!(u16::from_le_bytes([all[28], all[29]]), 0);
}

/// Only the first member name of the archive is read.
#[test]
fn reading_profile_files() {
    let (layouts, model) = meta();
    let m = ExportMetadata { motion_model: model, shaker_layout: "6 Speakers - 5.1", builtin_layouts: &layouts };
    let (_, bytes) = export_profile(&store(), Catalog::builtin(), "ac", &Vehicle::Unknown, None, &m).unwrap();
    let file = read_profile_file(&bytes).unwrap();
    assert_eq!(file.game, FileGame::Id("ac".into()));
    assert_eq!(file.vehicle, FileVehicle::Game);

    assert_eq!(read_profile_file(b"not a zip"), Err(ProfileFileError::NotAProfile));
    let zip_of = |members: &[(&str, &[u8])]| {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in members {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(content).unwrap();
        }
        w.finish().unwrap().into_inner()
    };
    let no_version = zip_of(&[("a.srs", br#"{"game": "ac", "vehicle": 0}"#)]);
    assert_eq!(read_profile_file(&no_version), Err(ProfileFileError::NotAProfile));
    let first_invalid =
        zip_of(&[("bad.txt", b"\xff\xfe"), ("b.srs", br#"{"game": "ac", "vehicle": "bmw", "srs_version": "x"}"#)]);
    assert_eq!(read_profile_file(&first_invalid), Err(ProfileFileError::NotAProfile));
    let first_good = zip_of(&[("b.srs", br#"{"game": "ac", "vehicle": 0, "srs_version": "x"}"#), ("bad.txt", b"\xff")]);
    assert_eq!(read_profile_file(&first_good).unwrap().vehicle, FileVehicle::Game);
    let two_profiles = zip_of(&[
        ("first.srs", br#"{"game":"ac","vehicle":"first car","srs_version":"x"}"#),
        ("second.srs", br#"{"game":"ac","vehicle":"second car","srs_version":"x"}"#),
    ]);
    assert_eq!(read_profile_file(&two_profiles).unwrap().vehicle, FileVehicle::Named("first car".into()));
    assert_eq!(read_profile_file(&zip_of(&[])), Err(ProfileFileError::NotAProfile));
    let no_vehicle = zip_of(&[("a.srs", br#"{"game": "ac", "srs_version": "x"}"#)]);
    assert_eq!(read_profile_file(&no_vehicle), Err(ProfileFileError::NotAProfile));
    let not_a_dict = zip_of(&[("a.srs", b"[1, 2]")]);
    assert_eq!(read_profile_file(&not_a_dict), Err(ProfileFileError::NotAProfile));
    // JSON parsing: NaN and Infinity are numbers, a repeated key keeps the last value, -0 is int 0.
    let python_json = zip_of(&[(
        "a.srs",
        br#"{"game": "ac", "vehicle": "x", "vehicle": -0, "srs_version": "x", "gear_shift_perc": NaN, "xpo_angle": Infinity}"#,
    )]);
    let file = read_profile_file(&python_json).unwrap();
    assert_eq!(file.vehicle, FileVehicle::Game);
    let nan = zip_of(&[("a.srs", br#"{"game": "ac", "vehicle": NaN, "srs_version": "x"}"#)]);
    assert_eq!(read_profile_file(&nan).unwrap().vehicle, FileVehicle::Other("nan".into()));
    // A UTF-8 BOM decodes to U+FEFF, which the JSON parser refuses.
    let bom = zip_of(&[("a.srs", b"\xef\xbb\xbf{\"game\": \"ac\", \"vehicle\": 0, \"srs_version\": \"x\"}")]);
    assert_eq!(read_profile_file(&bom), Err(ProfileFileError::NotAProfile));
}

/// The `game`/`vehicle` values as the import offer and the confirmed import compare them:
/// 0, 0.0 and false equal zero; the other scalars are keyed by their text form.
#[test]
fn profile_file_value_kinds() {
    let kinds = |game: serde_json::Value, vehicle: serde_json::Value| {
        let file = profile_file(game, vehicle, &factory("ac"));
        (file.game, file.vehicle)
    };
    use serde_json::json;
    assert_eq!(kinds(json!("ac"), json!(0)), (FileGame::Id("ac".into()), FileVehicle::Game));
    assert_eq!(kinds(json!("ac"), json!(0.0)).1, FileVehicle::Game);
    assert_eq!(kinds(json!("ac"), json!(false)).1, FileVehicle::Game);
    assert_eq!(kinds(json!("ac"), json!("0")).1, FileVehicle::Named("0".into()));
    assert_eq!(kinds(json!("ac"), json!(5)).1, FileVehicle::Other("5".into()));
    assert_eq!(kinds(json!("ac"), json!(2.5)).1, FileVehicle::Other("2.5".into()));
    assert_eq!(kinds(json!("ac"), json!(true)).1, FileVehicle::Other("True".into()));
    assert_eq!(kinds(json!("ac"), json!(null)).1, FileVehicle::Other("None".into()));
    assert_eq!(kinds(json!("ac"), json!([1])).1, FileVehicle::Unhashable);
    assert_eq!(kinds(json!(null), json!(0)).0, FileGame::Null);
    assert_eq!(kinds(json!(7), json!(0)).0, FileGame::Other);
}

fn profile_file(game: impl Into<serde_json::Value>, vehicle: impl Into<serde_json::Value>, tune: &Tune) -> ProfileFile {
    let mut json = tune.to_json();
    let map = json.as_object_mut().unwrap();
    map.insert("game".into(), game.into());
    map.insert("vehicle".into(), vehicle.into());
    map.insert("srs_version".into(), "2.13.0.0".into());
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file("x.srs", zip::write::SimpleFileOptions::default()).unwrap();
    w.write_all(json.to_string().as_bytes()).unwrap();
    read_profile_file(&w.finish().unwrap().into_inner()).unwrap()
}

/// The match rules of the import offer.
#[test]
fn import_must_match_the_profile_in_use() {
    let file = profile_file("ac", "bmw", &factory("ac"));
    let check = |game: &str, vehicle: Vehicle, forced: Option<&str>| {
        file.check(&ImportCheck { game_running: game, vehicle: &vehicle, forced_game: forced })
    };
    assert_eq!(check("none", Vehicle::Unknown, None), Ok(()));
    assert_eq!(check("ac", car("bmw"), None), Ok(()));
    assert_eq!(check("emulator", car("bmw"), Some("ac")), Ok(()));
    assert_eq!(check("ac", car("audi"), None), Err(ProfileFileError::NotInUse));
    assert_eq!(check("acc", car("bmw"), None), Err(ProfileFileError::NotInUse));
    assert_eq!(check("emulator", car("bmw"), Some("acc")), Err(ProfileFileError::NotInUse));

    let numeric = profile_file("ac", 5, &factory("ac"));
    let check_numeric = |game: &str, vehicle: Vehicle| {
        numeric.check(&ImportCheck { game_running: game, vehicle: &vehicle, forced_game: None })
    };
    assert_eq!(check_numeric("none", Vehicle::Unknown), Ok(()), "no game running: always offered");
    assert_eq!(check_numeric("ac", car("5")), Err(ProfileFileError::NotInUse), "5 != '5'");
    assert_eq!(check_numeric("ac", Vehicle::Unknown), Err(ProfileFileError::NotInUse), "5 != 0");

    let null_game = profile_file(serde_json::Value::Null, 0, &factory("ac"));
    let null_check = |forced: Option<&str>| {
        null_game.check(&ImportCheck { game_running: "emulator", vehicle: &Vehicle::Unknown, forced_game: forced })
    };
    assert_eq!(null_check(None), Ok(()), "force_game_detection None == game None");
    assert_eq!(null_check(Some("ac")), Err(ProfileFileError::NotInUse));
    assert_eq!(
        ProfileFileError::NotInUse.to_string(),
        "Import refused: the profile in this file does not correspond to the profile now in use."
    );
}

#[test]
fn native_import_offer_compares_values_without_hashing_or_stringifying() {
    use serde_json::json;
    for (file_vehicle, current) in [
        (json!(null), LegacyValue::None),
        (json!(1.0), LegacyValue::Bool(true)),
        (json!([7]), LegacyValue::List(vec![LegacyValue::Float(7.0)])),
        (json!({"id": 7}), LegacyValue::from(json!({"id": 7.0}))),
    ] {
        let file = profile_file("ac", file_vehicle.clone(), &factory("ac"));
        assert_eq!(file.check_native("ac", &current, None), Ok(()));
        assert_eq!(file.check_native("emulator", &current, Some("ac")), Ok(()));
        let text = LegacyValue::Str(openrigcue_model::python_repr::str_value(&current));
        assert_eq!(file.check_native("ac", &text, None), Err(ProfileFileError::NotInUse), "{file_vehicle}");
        assert_eq!(file.check_native("acc", &current, None), Err(ProfileFileError::NotInUse));
        assert_eq!(file.check_native("none", &text, None), Ok(()));
    }
    let null_game = profile_file(json!(null), json!(null), &factory("ac"));
    assert_eq!(null_game.check_native("emulator", &LegacyValue::None, None), Ok(()));
    let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
    archive.start_file("x.srs", zip::write::SimpleFileOptions::default()).unwrap();
    archive.write_all(br#"{"game":"ac","vehicle":NaN,"srs_version":"x"}"#).unwrap();
    let nan = read_profile_file(&archive.finish().unwrap().into_inner()).unwrap();
    assert_eq!(nan.check_native("ac", &LegacyValue::Float(f64::NAN), None), Err(ProfileFileError::NotInUse));
}

/// Confirmed import: old profiles get touchdown/turbulence and the front traction-loss family.
#[test]
fn import_backfills_and_stores() {
    let mut old = factory("ac");
    old.shaker_effects.touchdown = 3.0;
    let mut json = old.to_json();
    for group in ["shaker_effects", "shaker_dsp_type", "shaker_cutoff", "shaker_resonance", "shaker_switch"] {
        json[group].as_object_mut().unwrap().remove("touchdown");
        json[group].as_object_mut().unwrap().remove("turbulence");
    }
    for group in ["motion_effects", "motion_switch", "max_telemetry_vars", "motion_effect_smoothing_factor"] {
        json[group].as_object_mut().unwrap().remove("traction_loss_front");
    }
    json.as_object_mut().unwrap().remove("motion_main_actuators_traction_loss_front");
    json.as_object_mut().unwrap().remove("min_traction_loss_front_activation");
    json["motion_main_actuators_traction_loss"] = 12.into();
    json["motion_effects"]["traction_loss"] = 9.into();
    let old = Tune::from_legacy(&LegacyValue::from(json.clone()));
    assert!(old.is_err(), "old shape is not a complete tune");
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let mut with_meta = json;
    with_meta["game"] = "api".into();
    with_meta["vehicle"] = "Torque Drift".into();
    with_meta["srs_version"] = "2.13.0.0".into();
    with_meta["motion_model"] = "P3".into();
    w.start_file("x.srs", zip::write::SimpleFileOptions::default()).unwrap();
    w.write_all(with_meta.to_string().as_bytes()).unwrap();
    let file = read_profile_file(&w.finish().unwrap().into_inner()).unwrap();

    let mut s = store();
    assert_eq!(file.import_into(&mut s), Ok(SaveTarget::Vehicle("Torque Drift".into())));
    let imported = &s.get("api").unwrap().vehicles["Torque Drift"];
    assert_eq!(imported.shaker_effects.touchdown, 10.0);
    assert_eq!(imported.shaker_cutoff.turbulence, 400.0);
    assert_eq!(imported.motion_main_actuators_traction_loss_front, 12.0);
    assert_eq!(imported.min_traction_loss_front_activation, 12.0);
    assert_eq!(imported.motion_effects.traction_loss_front, 9.0);
    assert!(imported.game.is_none() && imported.srs_version.is_none());

    let game_file = profile_file("api", 0, &factory("ac"));
    assert_eq!(game_file.import_into(&mut s), Ok(SaveTarget::Game));
    assert_eq!(s.get("api").unwrap().gear_shift_perc, factory("ac").gear_shift_perc);
    assert_eq!(s.get("api").unwrap().vehicles.len(), 4, "stored vehicle tunes kept");

    let unknown = profile_file("iracing", 0, &factory("ac"));
    assert_eq!(unknown.import_into(&mut s), Err(ProfileFileError::Unreadable));
}

/// The confirmed import looks the store up with the file's raw `game`: the stand-by names that
/// a listener's game maps to `emulator` are missing keys here, for both branches.
#[test]
fn import_uses_the_raw_game_key() {
    let mut s = store();
    for game in ["none", "", "test"] {
        for vehicle in [serde_json::json!(0), serde_json::json!("bmw")] {
            let file = profile_file(game, vehicle, &factory("ac"));
            assert_eq!(file.import_into(&mut s), Err(ProfileFileError::Unreadable), "{game:?}");
        }
    }
    assert_eq!(s, store(), "the emulator profile is untouched");
    for game in [serde_json::Value::Null, serde_json::json!(7)] {
        assert_eq!(profile_file(game, 0, &factory("ac")).import_into(&mut s), Err(ProfileFileError::Unreadable));
    }
}

/// A confirmed import with a non-zero numeric vehicle stores a vehicle tune under that key (its
/// text here); an unhashable vehicle fails without changing the store.
#[test]
fn import_with_non_string_vehicles() {
    let mut s = store();
    let numeric = profile_file("ac", 5, &factory("api"));
    assert_eq!(numeric.import_into(&mut s), Ok(SaveTarget::Vehicle("5".into())));
    assert_eq!(s.get("ac").unwrap().vehicles["5"].gear_shift_perc, factory("api").gear_shift_perc);
    let before = s.clone();
    let list = profile_file("ac", serde_json::json!([1]), &factory("api"));
    assert_eq!(list.import_into(&mut s), Err(ProfileFileError::Unreadable));
    assert_eq!(s, before);
}

#[test]
fn integer_import_keeps_a_separate_key_and_existing_numeric_aliases() {
    let mut s = store();
    let mut named = factory("ac");
    named.gear_shift_perc = 77.0;
    s.save("ac", &SaveTarget::Vehicle("5".into()), named).unwrap();
    let mut imported = factory("ac");
    imported.gear_shift_perc = 43.0;
    profile_file("ac", 5, &imported).import_into(&mut s).unwrap();
    let rows = s.native_table();
    let vehicles = rows.get("ac").unwrap().as_dict().unwrap().get("vehicles").unwrap().as_dict().unwrap();
    assert_eq!(vehicles.get("5").unwrap().as_dict().unwrap().get("gear_shift_perc").unwrap().py_float(), Some(77.0));
    assert_eq!(
        vehicles.get_key(&Key::Int(5)).unwrap().as_dict().unwrap().get("gear_shift_perc").unwrap().py_float(),
        Some(43.0)
    );
    imported.gear_shift_perc = 31.0;
    profile_file("ac", 5.0, &imported).import_into(&mut s).unwrap();
    let rows = s.native_table();
    let vehicles = rows.get("ac").unwrap().as_dict().unwrap().get("vehicles").unwrap().as_dict().unwrap();
    assert_eq!(
        vehicles.get_key(&Key::Int(5)).unwrap().as_dict().unwrap().get("gear_shift_perc").unwrap().py_float(),
        Some(31.0)
    );
    assert!(!vehicles.contains("5.0"), "an existing integer key keeps its kind after equal-float assignment");
    profile_file("ac", 1, &factory("ac")).import_into(&mut s).unwrap();
    profile_file("ac", true, &imported).import_into(&mut s).unwrap();
    let rows = s.native_table();
    let vehicles = rows.get("ac").unwrap().as_dict().unwrap().get("vehicles").unwrap().as_dict().unwrap();
    assert_eq!(
        vehicles.get_key(&Key::Int(1)).unwrap().as_dict().unwrap().get("gear_shift_perc").unwrap().py_float(),
        Some(31.0)
    );
    assert!(!vehicles.contains("True"));
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("profile.db");
    s.save_all(&database).unwrap();
    assert_eq!(crate::import::sqlitedict::read_table(&database, "unnamed").unwrap(), rows);
}

#[test]
fn float_import_keeps_native_keys_and_the_original_zero_game_branch() {
    let mut s = store();
    for (vehicle, gear) in [
        (LegacyValue::Float(1.5), 43),
        ("1.5".into(), 61),
        (LegacyValue::Float(1.0), 45),
        (LegacyValue::Bool(true), 47),
        (LegacyValue::Int(1), 49),
        (LegacyValue::Float(f64::INFINITY), 53),
        ("inf".into(), 71),
    ] {
        let mut content = LegacyValue::from(factory("ac").to_json());
        let fields = content.as_dict_mut().unwrap();
        fields.insert("game", "ac".into());
        fields.insert("vehicle", vehicle);
        fields.insert("srs_version", "fixture".into());
        fields.insert("gear_shift_perc", LegacyValue::Int(gear));
        let text = crate::import::de::native_to_python_json(&content).unwrap();
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive.start_file("fixture.srs", zip::write::SimpleFileOptions::default()).unwrap();
        archive.write_all(text.as_bytes()).unwrap();
        read_profile_file(&archive.finish().unwrap().into_inner()).unwrap().import_into(&mut s).unwrap();
    }
    let rows = s.native_table();
    let vehicles = rows.get("ac").unwrap().item("vehicles").unwrap().as_dict().unwrap();
    assert_eq!(
        vehicles.keys().cloned().collect::<Vec<_>>(),
        [
            Key::float(1.5).unwrap(),
            Key::from("1.5"),
            Key::float(1.0).unwrap(),
            Key::float(f64::INFINITY).unwrap(),
            Key::from("inf")
        ]
    );
    for (key, gear) in [
        (Key::float(1.5).unwrap(), 43),
        (Key::from("1.5"), 61),
        (Key::Bool(true), 49),
        (Key::float(f64::INFINITY).unwrap(), 53),
        (Key::from("inf"), 71),
    ] {
        assert_eq!(vehicles.get_key(&key).unwrap().item("gear_shift_perc").unwrap(), &LegacyValue::Int(gear));
    }
    let mut game = factory("ac");
    game.gear_shift_perc = 29.0;
    assert_eq!(profile_file("ac", -0.0, &game).import_into(&mut s).unwrap(), SaveTarget::Game);
    let after = s.native_table();
    assert_eq!(after.get("ac").unwrap().item("vehicles").unwrap(), rows.get("ac").unwrap().item("vehicles").unwrap());
    assert_eq!(after.get("ac").unwrap().item("gear_shift_perc").unwrap().py_float(), Some(29.0));
}

#[test]
fn boolean_import_retains_its_key_kind_across_numeric_aliases_and_false_game_import() {
    let mut s = store();
    let mut imported = factory("ac");
    imported.gear_shift_perc = 43.0;
    profile_file("ac", true, &imported).import_into(&mut s).unwrap();
    let mut named = imported.clone();
    named.gear_shift_perc = 61.0;
    profile_file("ac", "True", &named).import_into(&mut s).unwrap();
    for vehicle in [serde_json::json!(1), serde_json::json!(1.0)] {
        imported.gear_shift_perc += 1.0;
        profile_file("ac", vehicle, &imported).import_into(&mut s).unwrap();
        let rows = s.native_table();
        let vehicles = rows.get("ac").unwrap().item("vehicles").unwrap().as_dict().unwrap();
        assert_eq!(vehicles.keys().cloned().collect::<Vec<_>>(), [Key::Bool(true), Key::from("True")]);
        assert_eq!(
            vehicles.get_key(&Key::Int(1)).unwrap().item("gear_shift_perc").unwrap().py_float(),
            Some(imported.gear_shift_perc)
        );
        assert_eq!(vehicles.get("True").unwrap().item("gear_shift_perc").unwrap().py_float(), Some(61.0));
    }
    let mut game = factory("ac");
    game.gear_shift_perc = 29.0;
    assert_eq!(profile_file("ac", false, &game).import_into(&mut s).unwrap(), SaveTarget::Game);
    let rows = s.native_table();
    let row = rows.get("ac").unwrap();
    assert_eq!(row.item("gear_shift_perc").unwrap().py_float(), Some(29.0));
    assert_eq!(
        row.item("vehicles").unwrap().as_dict().unwrap().keys().cloned().collect::<Vec<_>>(),
        [Key::Bool(true), Key::from("True")]
    );
}

/// Export All: nested archives for catalog games only.
#[test]
fn export_all_nests_archives() {
    let (layouts, model) = meta();
    let m = ExportMetadata { motion_model: model, shaker_layout: "SRS UShake6", builtin_layouts: &layouts };
    let bytes = export_all(&store(), Catalog::builtin(), &m).unwrap();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut names: Vec<String> = (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().to_owned()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "ac.srs",
            "api.srs",
            "api_Epic Roller Coasters.srs",
            "api_Flashout 3.srs",
            "api_Fly Dangerous.srs",
            "api_Torque Drift.srs",
            "emulator.srs"
        ]
    );
    let mut inner = Vec::new();
    archive.by_name("api_Flashout 3.srs").unwrap().read_to_end(&mut inner).unwrap();
    let file = read_profile_file(&inner).unwrap();
    assert_eq!(file.vehicle, FileVehicle::Named("Flashout 3".into()));
}

#[test]
fn profile_interchange_keeps_raw_types_missing_fields_and_database_order() {
    let mut s = store();
    let mut raw = s.resolve_native("ac", &Vehicle::Unknown).unwrap().0;
    let fields = raw.as_dict_mut().unwrap();
    fields.remove("motion_reaction_speed");
    fields.insert("gear_shift_perc", 71.into());
    fields.insert("custom", LegacyValue::List(vec![LegacyValue::None, LegacyValue::Float(f64::INFINITY)]));
    s.set_native_game("ac", raw);
    let (layouts, model) = meta();
    let m = ExportMetadata { motion_model: model, shaker_layout: "6 Speakers - 5.1", builtin_layouts: &layouts };
    let (_, bytes) = export_profile(&s, Catalog::builtin(), "ac", &Vehicle::Unknown, None, &m).unwrap();
    let text = String::from_utf8(unzip_single(&bytes).1).unwrap();
    assert!(text.contains("\"gear_shift_perc\": 71,"));
    assert!(!text.contains("motion_reaction_speed"));
    assert!(text.contains("Infinity"));
    let imported = read_profile_file(&bytes).unwrap();
    let mut target = store();
    imported.import_into(&mut target).unwrap();
    let after = target.resolve_native("ac", &Vehicle::Unknown).unwrap().0;
    assert_eq!(after.as_dict().unwrap().get("gear_shift_perc"), Some(&LegacyValue::Int(71)));
    assert!(after.as_dict().unwrap().get("motion_reaction_speed").is_none());
    assert_eq!(
        after.as_dict().unwrap().get("custom"),
        s.resolve_native("ac", &Vehicle::Unknown).unwrap().0.as_dict().unwrap().get("custom")
    );
    let mut vehicle = s.resolve_native("api", &Vehicle::Unknown).unwrap().0;
    vehicle.as_dict_mut().unwrap().insert("vehicles", Dict::new().into());
    s.save_native("ac", &SaveTarget::Vehicle("Z-last".into()), vehicle.clone()).unwrap();
    s.save_native("ac", &SaveTarget::Vehicle("A-first".into()), vehicle).unwrap();
    let bytes = export_all(&s, Catalog::builtin(), &m).unwrap();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let names: Vec<_> = (0..archive.len()).map(|index| archive.by_index(index).unwrap().name().to_owned()).collect();
    let stored_order = s.native_table().str_keys();
    assert_eq!(names.first(), Some(&format!("{}.srs", stored_order[0])));
    assert!(
        names.iter().position(|name| name == "ac_Z-last.srs").unwrap()
            < names.iter().position(|name| name == "ac_A-first.srs").unwrap()
    );
}

/// Single-profile export: the catalog name is read before the
/// file is produced, so a stored game outside the catalog fails; a game missing from the store
/// fails first.
#[test]
fn export_profile_requires_a_catalog_game() {
    let (layouts, model) = meta();
    let m = ExportMetadata { motion_model: model, shaker_layout: "SRS UShake6", builtin_layouts: &layouts };
    let s = store();
    assert!(s.get("condor3").is_some() && !Catalog::builtin().contains("condor3"));
    assert_eq!(
        export_profile(&s, Catalog::builtin(), "condor3", &Vehicle::Unknown, None, &m),
        Err(ProfileError::NotInCatalog("condor3".into()))
    );
    assert_eq!(
        export_profile(&s, Catalog::builtin(), "nope", &Vehicle::Unknown, None, &m),
        Err(ProfileError::UnknownGame("nope".into()))
    );
}

fn full_store() -> ProfileStore {
    let mut s = ProfileStore::default();
    for (game, tune) in FactoryTunes::builtin().games() {
        s.insert(game, tune.clone());
    }
    s
}

fn listed_ids<'a>(entries: &[ManagerEntry<'a>]) -> Vec<&'a str> {
    entries.iter().map(|e| e.game.id.as_str()).collect()
}

/// The Profile Manager listing on Windows: every catalog game except ccd,
/// sorted by lower-cased name ("American Truck Simulator" before "API"), vehicles sorted.
#[test]
fn manager_entries_on_windows() {
    let s = full_store();
    let entries = s.manager_entries(Catalog::builtin(), Platform::Windows).unwrap();
    let ids = listed_ids(&entries);
    assert_eq!(ids.len(), 125);
    assert_eq!(ids[..6], ["aceshigh", "acesofthunder", "aeroflyfs2", "ats", "api", "arca"]);
    assert_eq!(ids[ids.len() - 3..], ["wrcg", "wreckfest2", "xplane"]);
    let api = entries.iter().find(|e| e.game.id == "api").unwrap();
    assert_eq!(api.vehicles, ["Epic Roller Coasters", "Flashout 3", "Fly Dangerous", "Torque Drift"]);
}

#[test]
fn profile_manager_lists_vehicle_keys_even_when_their_values_cannot_be_typed() {
    let mut s = full_store();
    s.save_native("ac", &SaveTarget::Vehicle("Unassigned".into()), LegacyValue::None).unwrap();
    s.save_native("ac", &SaveTarget::Vehicle("Empty".into()), Dict::new().into()).unwrap();
    let entries = s.manager_entries(Catalog::builtin(), Platform::Windows).unwrap();
    let ac = entries.iter().find(|entry| entry.game.id == "ac").unwrap();
    assert_eq!(ac.vehicles, ["Empty", "Unassigned"]);
}

/// The listing off Windows: only games whose platforms list Console.
#[test]
fn manager_entries_off_windows_list_console_games_only() {
    let s = full_store();
    let entries = s.manager_entries(Catalog::builtin(), Platform::Linux).unwrap();
    assert_eq!(
        listed_ids(&entries),
        [
            "api",
            "acudp",
            "f12016",
            "f12017",
            "f12018",
            "f12019",
            "f12020",
            "f12021",
            "f12022",
            "f12023",
            "f12024",
            "f12025",
            "forzahorizon4",
            "forzahorizon5",
            "forzahorizon6",
            "forzamotorsports7",
            "forzamotorsports8",
            "gt7",
            "gtsport",
            "projectcars2udp",
            "projectcarsudp",
        ]
    );
}

/// The listing orders a game's vehicles by code point.
#[test]
fn manager_entries_sort_vehicles_by_code_point() {
    let mut s = full_store();
    let mut ac = s.get("ac").unwrap().clone();
    ac.vehicles.clear();
    for name in ["b", "B", "a", "Ä"] {
        ac.vehicles.insert(name.into(), factory("ac"));
    }
    s.insert("ac", ac);
    let entries = s.manager_entries(Catalog::builtin(), Platform::Windows).unwrap();
    assert_eq!(entries.iter().find(|e| e.game.id == "ac").unwrap().vehicles, ["B", "a", "b", "Ä"]);
}

/// The listing reads the stored row of every catalog game before the skip rules, so the
/// first missing one in listing order fails the listing.
#[test]
fn manager_entries_need_every_catalog_game() {
    let mut s = full_store();
    s.games.remove("xplane");
    s.games.remove("aceshigh");
    assert_eq!(
        s.manager_entries(Catalog::builtin(), Platform::Linux),
        Err(ProfileError::UnknownGame("aceshigh".into()))
    );
}

#[test]
fn typed_signed_zero_edit_reaches_the_native_profile_and_saved_row() {
    let mut profile = factory("ac");
    profile.motion_effects.pitch = 0.0;
    let mut profiles = ProfileStore::new([("ac".to_owned(), profile)].into_iter().collect());
    profiles.games.get_mut("ac").unwrap().motion_effects.pitch = -0.0;
    let raw = profiles.native_table();
    let pitch = raw.get("ac").unwrap().item("motion_effects").unwrap().item("pitch").unwrap();
    let LegacyValue::Float(pitch) = pitch else { panic!("float pitch") };
    assert_eq!(pitch.to_bits(), (-0.0_f64).to_bits());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.db");
    profiles.save_all(&path).unwrap();
    let saved = crate::import::sqlitedict::read_item(&path, "unnamed", "ac").unwrap().unwrap();
    let LegacyValue::Float(pitch) = saved.item("motion_effects").unwrap().item("pitch").unwrap() else {
        panic!("saved float")
    };
    assert_eq!(pitch.to_bits(), (-0.0_f64).to_bits());
}

/// A stored row can be an object its saver still holds (a game save assigns the saved dictionary itself).
/// Reading the table with a typed edit outstanding shows the edit and leaves that object as it was, so the
/// typed field set back to its earlier value reads as that value again.
#[test]
fn reading_the_native_table_writes_no_typed_edit_into_a_shared_row() {
    use openrigcue_model::native_value::NativeObject;
    const FIELDS: [&[&str]; 2] = [&["gear_shift_perc"], &["motion_effects", "pitch"]];
    let numbers = |row: &LegacyValue| {
        FIELDS
            .map(|path| path.iter().fold(row.clone(), |value, key| value.item_owned(key).unwrap()).py_float().unwrap())
    };
    let read = |s: &ProfileStore| numbers(s.native_table().get("ac").unwrap());
    let mut s = store();
    let row = LegacyValue::Object(NativeObject::new(LegacyValue::from(factory("ac").to_json())));
    s.save_native("ac", &SaveTarget::Game, row.clone()).unwrap();
    let saved = [factory("ac").gear_shift_perc, factory("ac").motion_effects.pitch];
    assert_eq!(read(&s), saved);
    let mut tune = factory("ac");
    tune.gear_shift_perc -= 5.0;
    tune.motion_effects.pitch -= 5.0;
    s.save("ac", &SaveTarget::Game, tune).unwrap();
    assert_eq!(read(&s), saved.map(|value| value - 5.0), "the typed edit is read");
    assert_eq!(numbers(&row), saved, "reading wrote into the shared row");
    s.save("ac", &SaveTarget::Game, factory("ac")).unwrap();
    assert_eq!(read(&s), saved, "the typed fields are back at their saved values");
}

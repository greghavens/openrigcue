use super::steps::{flight_level, Migrator, STEPS};
use super::*;
use crate::import::shipped::ShippedData;
use crate::import::value::Key;

#[test]
fn registry_migrations_read_live_values_and_commit_each_caught_write_independently() {
    use crate::import::sqlitedict;
    let legacy = LegacyLayouts::Absent;
    for rejected in [None, Some("ShowSplashScreen"), Some("StartWithWindows")] {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("registry.db");
        sqlitedict::set_item(&path, "registry", "StartWithWindows", &s("1")).unwrap();
        sqlitedict::set_item(&path, "registry", "ShowSplashScreen", &s("0")).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute("INSERT INTO registry (key,value) VALUES ('Opaque',?)", [vec![255_u8]]).unwrap();
        // Replacing an opaque selected value must not decode it first.
        if rejected.is_none() {
            connection.execute("UPDATE registry SET value=? WHERE key='StartWithWindows'", [vec![255_u8]]).unwrap();
        }
        if let Some(key) = rejected {
            connection.execute_batch(&format!(
                "CREATE TRIGGER reject_flag BEFORE INSERT ON registry WHEN NEW.key='{key}' BEGIN SELECT RAISE(ABORT,'registry write rejected'); END;"
            )).unwrap();
        }
        let mut state = LegacyState {
            config: config(vec![("APP", dict(vec![("start_with_windows", s("no"))]))]),
            ..Default::default()
        };
        let sources = MigrationSources {
            effects: Some(StartEffects { docs: temporary.path(), patch: None }),
            ..ShippedData::get().sources(Platform::Windows, &legacy)
        };
        let mut migrator = Migrator::new(&mut state, &sources).unwrap();
        migrator.current = 288;
        (STEPS.iter().find(|step| step.version == 288).unwrap().apply)(&mut migrator).unwrap();
        drop(migrator);
        assert!(!state.config.get("APP").unwrap().as_dict().unwrap().contains("start_with_windows"));
        for (key, before, after) in [("ShowSplashScreen", "0", "1"), ("StartWithWindows", "1", "0")] {
            assert_eq!(
                sqlitedict::read_item(&path, "registry", key).unwrap(),
                Some(s(if rejected == Some(key) { before } else { after }))
            );
            assert_eq!(state.registry.get(key), (rejected != Some(key)).then(|| s(after)).as_ref());
        }
        // v359 must consult the live registry, rather than fill values absent
        // from its in-memory typed snapshot.
        connection.execute_batch("DROP TRIGGER IF EXISTS reject_flag; CREATE TRIGGER reject_all BEFORE INSERT ON registry BEGIN SELECT RAISE(ABORT,'existing flags must not be rewritten'); END;").unwrap();
        state.registry = Dict::new();
        let before = std::fs::read(&path).unwrap();
        let mut migrator = Migrator::new(&mut state, &sources).unwrap();
        migrator.current = 359;
        (STEPS.iter().find(|step| step.version == 359).unwrap().apply)(&mut migrator).unwrap();
        drop(migrator);
        assert!(state.registry.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(sqlitedict::read_item(&path, "registry", "Opaque").is_err());
    }
}

#[test]
fn registry_defaults_commit_before_a_later_migration_error() {
    use crate::import::sqlitedict;
    for selected in [None, Some(LegacyValue::None), Some(s("opaque"))] {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("registry.db");
        sqlitedict::ensure_table(&path, "registry").unwrap();
        if let Some(value) = selected {
            sqlitedict::set_item(&path, "registry", "StartWithWindows", &value).unwrap();
            if value == s("opaque") {
                rusqlite::Connection::open(&path)
                    .unwrap()
                    .execute("UPDATE registry SET value=? WHERE key='StartWithWindows'", [vec![255_u8]])
                    .unwrap();
            }
        }
        let mut state = LegacyState {
            config: config(vec![("APP", dict(vec![("first_time_installation", s("invalid"))]))]),
            ..Default::default()
        };
        let legacy = LegacyLayouts::Absent;
        let sources = MigrationSources {
            effects: Some(StartEffects { docs: temporary.path(), patch: None }),
            ..ShippedData::get().sources(Platform::Windows, &legacy)
        };
        let mut migrator = Migrator::new(&mut state, &sources).unwrap();
        migrator.current = 359;
        assert!((STEPS.iter().find(|step| step.version == 359).unwrap().apply)(&mut migrator).is_err());
        drop(migrator);
        for key in ["StartWithWindows", "ShowSplashScreen"] {
            assert_eq!(sqlitedict::read_item(&path, "registry", key).unwrap(), Some(s("1")));
        }
    }
}

#[test]
fn first_install_hardware_migration_uses_current_selected_registry_values() {
    use crate::import::sqlitedict;
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("registry.db");
    sqlitedict::set_item(&path, "registry", "Soundcard", &s("Live USB speakers")).unwrap();
    sqlitedict::set_item(&path, "registry", "MotionModel", &s("H3")).unwrap();
    let mut state = LegacyState {
        config: config(vec![
            ("APP", dict(vec![("first_time_installation", s("yes"))])),
            ("SHAKEBOX", dict(vec![("soundcard", s("old"))])),
            ("MOTIONBOX", dict(vec![("model", s("old")), ("refresh_rate", s("1"))])),
        ]),
        registry: dict(vec![("Soundcard", s("cached")), ("MotionModel", s("cached"))]),
        ..Default::default()
    };
    let legacy = LegacyLayouts::Absent;
    let sources = MigrationSources {
        effects: Some(StartEffects { docs: temporary.path(), patch: None }),
        ..ShippedData::get().sources(Platform::Windows, &legacy)
    };
    let mut migrator = Migrator::new(&mut state, &sources).unwrap();
    migrator.current = 230;
    (STEPS.iter().find(|step| step.version == 230).unwrap().apply)(&mut migrator).unwrap();
    drop(migrator);
    assert_eq!(get(&state.config, &["SHAKEBOX", "soundcard"]), &s("Live USB speakers"));
    assert_eq!(get(&state.config, &["MOTIONBOX", "model"]), &s("H3"));
    assert_eq!(get(&state.config, &["MOTIONBOX", "refresh_rate"]), &LegacyValue::Int(30));
}

#[test]
fn metadata_migrations_use_live_rows_and_stop_decoding_at_the_matching_layout() {
    use crate::import::sqlitedict;
    let temporary = tempfile::tempdir().unwrap();
    let metadata = temporary.path().join("metadata.db");
    let cached = LegacyValue::from(serde_json::json!({"marker":"cached"}));
    let live = LegacyValue::from(serde_json::json!({"marker":"live"}));
    sqlitedict::set_item(&metadata, "shakers", "6 Speakers - 5.1", &live).unwrap();
    let mut state = LegacyState {
        config: config(vec![("CUSTOM_FEATURES", Dict::new())]),
        shaker_layouts: [(Key::from("6 Speakers - 5.1"), cached)].into_iter().collect(),
        ..Default::default()
    };
    let legacy = LegacyLayouts::Absent;
    let sources = MigrationSources {
        effects: Some(StartEffects { docs: temporary.path(), patch: None }),
        ..ShippedData::get().sources(Platform::Linux, &legacy)
    };
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.current = 264;
    (STEPS.iter().find(|step| step.version == 264).unwrap().apply)(&mut m).unwrap();
    assert_eq!(sqlitedict::read_item(&metadata, "shakers", "SRS UShake6").unwrap(), Some(live));
    drop(m);
    for version in [285, 342] {
        for before in [false, true] {
            for first in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("metadata.db");
                sqlitedict::ensure_table(&path, "shakers").unwrap();
                let db = rusqlite::Connection::open(&path).unwrap();
                let opaque =
                    || db.execute("INSERT INTO shakers (key,value) VALUES ('Opaque',?)", [vec![255_u8]]).unwrap();
                if before {
                    opaque();
                }
                let original = LegacyValue::from(
                    serde_json::json!({"marker":"live","CE":{"sfx":{"engine":1}},"SUB":{"sfx":{"engine":2}}}),
                );
                sqlitedict::set_item(&path, "shakers", "SRS UShake6", &original).unwrap();
                if !before {
                    opaque();
                }
                let cached = LegacyValue::from(
                    serde_json::json!({"marker":"cached","CE":{"sfx":{"engine":9}},"SUB":{"sfx":{"engine":9}}}),
                );
                let mut state = LegacyState {
                    config: config(vec![]),
                    shaker_layouts: [(Key::from("SRS UShake6"), cached)].into_iter().collect(),
                    ..Default::default()
                };
                state
                    .config
                    .get_mut("APP")
                    .unwrap()
                    .as_dict_mut()
                    .unwrap()
                    .insert("first_time_installation", s(if first { "yes" } else { "no" }));
                let sources = MigrationSources {
                    effects: Some(StartEffects { docs: temp.path(), patch: None }),
                    ..ShippedData::get().sources(Platform::Linux, &legacy)
                };
                let mut m = Migrator::new(&mut state, &sources).unwrap();
                m.current = version;
                let result = (STEPS.iter().find(|step| step.version == version).unwrap().apply)(&mut m);
                assert_eq!(result.is_err(), first && before, "v{version} first={first} before={before}: {result:?}");
                let stored = sqlitedict::read_item(&path, "shakers", "SRS UShake6").unwrap().unwrap();
                let mut expected = original;
                if first && !before {
                    let level = if version == 285 { 6 } else { 4 };
                    for channel in if version == 285 { vec!["CE"] } else { vec!["CE", "SUB"] } {
                        expected
                            .as_dict_mut()
                            .unwrap()
                            .get_mut(channel)
                            .unwrap()
                            .as_dict_mut()
                            .unwrap()
                            .get_mut("sfx")
                            .unwrap()
                            .as_dict_mut()
                            .unwrap()
                            .insert("engine", LegacyValue::Int(level));
                    }
                }
                assert_eq!(stored, expected, "v{version} first={first} before={before}");
            }
        }
    }
}

fn s(v: &str) -> LegacyValue {
    LegacyValue::Str(v.into())
}

fn dict(entries: Vec<(&str, LegacyValue)>) -> Dict {
    entries.into_iter().map(|(k, v)| (Key::from(k), v)).collect()
}

fn config(sections: Vec<(&str, Dict)>) -> Dict {
    let mut d = dict(sections.into_iter().map(|(k, v)| (k, LegacyValue::Dict(v))).collect());
    if !d.contains("APP") {
        d.insert("APP", LegacyValue::Dict(Dict::new()));
    }
    if let Some(LegacyValue::Dict(app)) = d.get_mut("APP") {
        app.set_default("config_file_version", s("83"));
        app.set_default("first_time_installation", s("no"));
    }
    d
}

/// Applies the single step `version` to `state`.
fn apply(version: u32, state: &mut LegacyState, platform: Platform) -> Result<MigrationReport, String> {
    let data = ShippedData::get();
    let no_legacy_layouts = LegacyLayouts::Absent;
    let sources = data.sources(platform, &no_legacy_layouts);
    let mut m = Migrator::new(state, &sources).map_err(|e| e.message)?;
    m.current = version;
    let step = STEPS.iter().find(|s| s.version == version).expect("step exists");
    (step.apply)(&mut m)?;
    Ok(m.report)
}

fn get<'a>(d: &'a Dict, path: &[&str]) -> &'a LegacyValue {
    let mut v = d.get(path[0]).unwrap_or_else(|| panic!("{path:?}"));
    for p in &path[1..] {
        v = v.as_dict().unwrap().get(p).unwrap_or_else(|| panic!("{path:?}"));
    }
    v
}

fn fresh() -> (LegacyState, MigrationReport) {
    let data = ShippedData::get();
    let mut state = data.fresh_install_state();
    let report = migrate(&mut state, &data.sources(Platform::Linux, &LegacyLayouts::Absent)).unwrap();
    (state, report)
}

fn tune_with(entries: Vec<(&str, LegacyValue)>) -> LegacyValue {
    let mut d = dict(entries);
    d.set_default("vehicles", LegacyValue::Dict(Dict::new()));
    LegacyValue::Dict(d)
}

/// The fresh-install chain ends with the factory tunes except where steps
/// copy one game's factory profile into another or set vehicle values differently.
#[test]
fn fresh_install_matches_factory_tunes_except_original_quirks() {
    let (state, report) = fresh();
    assert_eq!(report.from_version, 84.into(), "template.ini");
    assert_eq!(report.applied.first(), Some(&85));
    assert_eq!(report.applied.last(), Some(&412));
    let master = &ShippedData::get().master_tunes;
    let mut diffs = Vec::new();
    fn walk(a: &LegacyValue, b: Option<&LegacyValue>, path: String, out: &mut Vec<String>) {
        match (a, b) {
            (LegacyValue::Dict(x), Some(LegacyValue::Dict(y))) => {
                for (k, v) in x.iter() {
                    walk(v, y.get_key(k), format!("{path}/{}", k.to_text()), out);
                }
                for (k, _) in y.iter() {
                    if x.get_key(k).is_none() {
                        out.push(format!("{path}/{} missing", k.to_text()));
                    }
                }
            }
            (_, None) => out.push(format!("{path} extra")),
            (x, Some(y)) => {
                let numeric_equal = matches!((x.py_float(), y.py_float()), (Some(p), Some(q)) if p == q)
                    && !matches!(x, LegacyValue::Str(_));
                if !numeric_equal && x != y {
                    out.push(format!("{path} {x} != {y}"));
                }
            }
        }
    }
    walk(
        &LegacyValue::Dict(state.profiles.clone()),
        Some(&LegacyValue::Dict(master.clone())),
        String::new(),
        &mut diffs,
    );
    diffs.sort();
    assert_eq!(
        diffs,
        [
            "/aceshigh/min_traction_loss_activation 30 != 0",
            "/api/vehicles/Epic Roller Coasters/intellitune_motion_preference None != ",
            "/api/vehicles/Epic Roller Coasters/min_traction_loss_activation 30 != 0",
            "/api/vehicles/Torque Drift/intellitune_motion_preference None != ",
            "/ed/vehicles/spaceship/intellitune_motion_preference None != ",
            "/ed/vehicles/srv/intellitune_motion_preference None != ",
            "/emulator/motion_effects/motion_switch extra",
            "/forzamotorsports8/min_traction_loss_activation 30 != 0",
            "/joy/vehicles/Default/intellitune_motion_preference None != ",
            "/rfpro/min_traction_loss_activation 30 != 0",
        ]
    );
}

/// v88/v300 write their version into INTELLIBOX, the version ends at 412 and the install flag is cleared.
#[test]
fn cursor_and_first_install_flag() {
    let (state, report) = fresh();
    assert_eq!(get(&state.config, &["APP", "config_file_version"]), &LegacyValue::Int(412));
    assert_eq!(get(&state.config, &["INTELLIBOX", "config_file_version"]), &LegacyValue::Int(300));
    assert_eq!(get(&state.config, &["APP", "first_time_installation"]), &s("no"));
    assert!(report.skipped_side_effects.iter().any(|e| e.starts_with("v214")));
    assert!(!report.applied.contains(&146) && !report.applied.contains(&192), "`if False` blocks");
}

#[test]
fn cursor_only_migration_is_retained_when_the_following_step_fails() {
    let mut state = LegacyState {
        config: config(vec![("APP", dict(vec![("config_file_version", s("404"))]))]),
        ..LegacyState::default()
    };
    let failure =
        migrate(&mut state, &ShippedData::get().sources(Platform::Linux, &LegacyLayouts::Absent)).unwrap_err();
    assert_eq!(failure.version, 406);
    assert_eq!(get(&state.config, &["APP", "config_file_version"]), &LegacyValue::Int(405));
    assert!(!state.config.contains("CUSTOM_FEATURES"));
}

#[test]
fn scn6_migration_changes_only_the_transient_startup_profile() {
    let mut state = LegacyState {
        config: config(vec![("APP", dict(vec![("config_file_version", s("285"))]))]),
        profiles: dict(vec![("emulator", tune_with(vec![("scn6_speed", 1250.into())]))]),
        ..LegacyState::default()
    };
    let saved = state.clone();
    let sources = ShippedData::get().sources(Platform::Linux, &LegacyLayouts::Absent);
    let mut migrator = Migrator::new(&mut state, &sources).unwrap();
    let step = STEPS.iter().find(|step| step.version == 286).unwrap();
    (step.apply)(&mut migrator).unwrap();
    assert_eq!(migrator.startup_game_config.get("scn6_speed"), Some(&LegacyValue::Int(5000)));
    assert_eq!(migrator.startup_game_config.get("scn6_acceleration"), Some(&LegacyValue::Int(80)));
    assert_eq!(*migrator.state, saved, "temporary globals must not become stored profile edits");
}

/// A file already at 412 runs no step.
#[test]
fn current_files_are_untouched() {
    let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", s("412"));
    let data = ShippedData::get();
    let report = migrate(&mut state, &data.sources(Platform::Windows, &LegacyLayouts::Absent)).unwrap();
    assert!(report.applied.is_empty());
}

/// A current file that carries the 4D video port, switch and game loses them; nothing else is touched.
#[test]
fn four_d_video_entries_are_dropped() {
    let mut state = LegacyState {
        config: config(vec![
            ("NETWORK_PORT", dict(vec![("dirt", s("30778")), ("4dvideo", s("45212"))])),
            ("CUSTOM_FEATURES", dict(vec![("4dvideo_editor_mode", s("yes")), ("tuner_mode", s("no"))])),
        ]),
        ..LegacyState::default()
    };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", s("412"));
    state.profiles.insert("4dvideo", LegacyValue::Dict(dict(vec![])));
    state.profiles.insert("ac", LegacyValue::Dict(dict(vec![])));
    let data = ShippedData::get();
    let report = migrate(&mut state, &data.sources(Platform::Linux, &LegacyLayouts::Absent)).unwrap();
    assert!(report.applied.is_empty());
    let ports = state.config.get("NETWORK_PORT").unwrap().as_dict().unwrap();
    assert!(ports.get("4dvideo").is_none());
    assert!(ports.get("dirt").is_some());
    let features = state.config.get("CUSTOM_FEATURES").unwrap().as_dict().unwrap();
    assert!(features.get("4dvideo_editor_mode").is_none());
    assert!(features.get("tuner_mode").is_some());
    assert!(!state.profiles.contains("4dvideo"));
    assert!(state.profiles.contains("ac"));
}

/// The chain fails without a usable `APP.config_file_version`.
#[test]
fn missing_or_bad_version_fails() {
    let data = ShippedData::get();
    let mut state = LegacyState::default();
    assert!(migrate(&mut state, &data.sources(Platform::Linux, &LegacyLayouts::Absent)).is_err());
    let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", s("4.1"));
    assert!(migrate(&mut state, &data.sources(Platform::Linux, &LegacyLayouts::Absent)).is_err());
}

/// Without `APP.first_time_installation` the chain fails before any step.
#[test]
fn missing_first_installation_flag_fails() {
    let data = ShippedData::get();
    let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().remove("first_time_installation");
    let err = migrate(&mut state, &data.sources(Platform::Windows, &LegacyLayouts::Absent)).unwrap_err();
    assert_eq!(err.version, 0);
    assert!(err.message.contains("first_time_installation"), "{err}");
}

/// v111: integer value / 10 for DOF Reality and / 100 for PROSIMU (float results).
#[test]
fn v111_rescales_thresholds() {
    let thresholds = |a: &str, b: &str, c: &str| {
        dict(vec![("smoothing_threshold", s(a)), ("soft_crash_threshold", s(b)), ("boost_threshold", s(c))])
    };
    let mut state = LegacyState {
        config: config(vec![
            ("DOF REALITY", thresholds("150", "500", "155")),
            ("PROSIMU", thresholds("1", "5000", "1500")),
        ]),
        ..LegacyState::default()
    };
    apply(111, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["DOF REALITY", "boost_threshold"]), &LegacyValue::Float(15.5));
    assert_eq!(get(&state.config, &["DOF REALITY", "soft_crash_threshold"]), &LegacyValue::Float(50.0));
    assert_eq!(get(&state.config, &["PROSIMU", "smoothing_threshold"]), &LegacyValue::Float(0.01));
    let mut state = LegacyState {
        config: config(vec![("DOF REALITY", thresholds("15.0", "1", "1")), ("PROSIMU", thresholds("1", "1", "1"))]),
        ..LegacyState::default()
    };
    assert!(apply(111, &mut state, Platform::Windows).is_err(), "int('15.0') raises");
}

/// MOTIONBOX.model renames: v161 `2 Surge` → `Surge + Belt`, v275 `DEMO` → `STR-SCN6150`, v347
/// PT-Actuator `(…)` → Thanos `[…]` (the table has no 4-actuator entry; CANBus names keep theirs).
#[test]
fn motion_model_renames() {
    let model = |m: &str| config(vec![("MOTIONBOX", dict(vec![("model", s(m))]))]);
    let renamed = |version: u32, m: &str| {
        let mut state = LegacyState { config: model(m), profiles: Dict::new(), ..LegacyState::default() };
        if version == 275 {
            state.config.insert(
                "SIMTECHREVOLUTION",
                LegacyValue::Dict(dict(vec![
                    ("stroke_mm", s("1")),
                    ("max_motor_abs", s("1")),
                    ("model_code", s("1")),
                    ("nbr_controllers", s("1")),
                    ("nbr_motors", s("1")),
                ])),
            );
        }
        apply(version, &mut state, Platform::Windows).unwrap();
        get(&state.config, &["MOTIONBOX", "model"]).as_str().unwrap().to_owned()
    };
    assert_eq!(
        renamed(161, "6 Actuators (2 Front + 2 Rear + 2 Surge)"),
        "6 Actuators (2 Front + 2 Rear + Surge + Belt)"
    );
    assert_eq!(renamed(275, "DEMO"), "STR-SCN6150");
    assert_eq!(renamed(275, "DEMO2"), "DEMO2");
    assert_eq!(renamed(347, "3 Actuators (2 Front + 1 Rear)"), "3 Actuators [2 Front + 1 Rear]");
    assert_eq!(
        renamed(347, "8 Actuators (2 Front + 2 Rear + 2 TL + Surge + Belt)"),
        "8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]"
    );
    assert_eq!(renamed(347, "4 Actuators (2 Front + 2 Rear)"), "4 Actuators (2 Front + 2 Rear)");
    assert_eq!(renamed(347, "3 Actuators (2 Front + 1 Rear) CANBus"), "3 Actuators (2 Front + 1 Rear) CANBus");
}

/// v275 creates SIMTECHREVOLUTION from PROSIMU without the PRS200 keys.
#[test]
fn v275_creates_simtech_from_prosimu() {
    let prosimu = dict(vec![
        ("smoothing_threshold", s("1")),
        ("prs200_firmware", s("3")),
        ("prs200_com_port1", s("")),
        ("prs200_com_port2", s("")),
        ("scn6_protocol", s("D")),
    ]);
    let mut state = LegacyState {
        config: config(vec![("PROSIMU", prosimu), ("MOTIONBOX", dict(vec![("model", s("P3"))]))]),
        ..LegacyState::default()
    };
    apply(275, &mut state, Platform::Windows).unwrap();
    assert_eq!(
        get(&state.config, &["SIMTECHREVOLUTION"]).as_dict().unwrap().str_keys(),
        ["smoothing_threshold", "scn6_protocol"]
    );
}

/// v222: flight-sim vehicle keys lose trailing NULs; a key with only an interior NUL maps to itself
/// and is deleted (assign, then delete).
#[test]
fn v222_strips_nul_from_vehicle_names() {
    let vehicles = dict(vec![
        ("Cessna\0\0", tune_with(vec![("a", 1.into())])),
        ("A\0B", tune_with(vec![("b", 2.into())])),
        ("Plain", tune_with(vec![("c", 3.into())])),
    ]);
    let mut state = LegacyState {
        config: config(vec![]),
        profiles: dict(vec![
            ("fsx", tune_with(vec![("vehicles", LegacyValue::Dict(vehicles.clone()))])),
            ("ac", tune_with(vec![("vehicles", LegacyValue::Dict(vehicles))])),
        ]),
        ..LegacyState::default()
    };
    apply(222, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.profiles, &["fsx", "vehicles"]).as_dict().unwrap().str_keys(), ["Plain", "Cessna"]);
    assert_eq!(get(&state.profiles, &["ac", "vehicles"]).as_dict().unwrap().len(), 3, "other games untouched");
}

/// v223: rFactor 2 vehicle keys are cut one character before the last `#`.
#[test]
fn v223_cuts_rf2_vehicle_names() {
    let vehicles = dict(vec![
        ("Formula Renault 3.5 #12", tune_with(vec![])),
        ("#7", tune_with(vec![])),
        ("No hash", tune_with(vec![])),
    ]);
    let mut state = LegacyState {
        config: config(vec![]),
        profiles: dict(vec![("rf2", tune_with(vec![("vehicles", LegacyValue::Dict(vehicles))]))]),
        ..LegacyState::default()
    };
    apply(223, &mut state, Platform::Windows).unwrap();
    assert_eq!(
        get(&state.profiles, &["rf2", "vehicles"]).as_dict().unwrap().str_keys(),
        ["No hash", "Formula Renault 3.5", "#"]
    );
}

/// v262 splits `surge` into acceleration/deceleration ranges and tolerates tunes without ranges.
#[test]
fn v262_splits_surge_range() {
    let ranges = LegacyValue::Dict(dict(vec![("pitch", 5.into()), ("surge", 3.into())]));
    let mut state = LegacyState {
        config: config(vec![]),
        profiles: dict(vec![("ac", tune_with(vec![("max_telemetry_vars", ranges)])), ("odd", tune_with(vec![]))]),
        ..LegacyState::default()
    };
    apply(262, &mut state, Platform::Windows).unwrap();
    let r = get(&state.profiles, &["ac", "max_telemetry_vars"]).as_dict().unwrap();
    assert_eq!(r.str_keys(), ["pitch", "surge_acceleration", "surge_deceleration"]);
    assert_eq!(r.get("surge_deceleration"), Some(&LegacyValue::Int(3)));
}

fn profile_for_v153() -> Dict {
    let vehicle = tune_with(vec![("motion_effect_smoothing_factor", LegacyValue::Dict(Dict::new()))]);
    dict(vec![
        ("ac", tune_with(vec![("vehicles", LegacyValue::Dict(dict(vec![("unpublished car", vehicle)])))])),
        ("rbr", tune_with(vec![])),
    ])
}

/// v153 on a first installation: boost 5, smoothing 20, and the jeffweaver ranges of published games.
#[test]
fn v153_first_install_imports_expert_ranges() {
    let mut state = LegacyState { config: config(vec![]), profiles: profile_for_v153(), ..LegacyState::default() };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("first_time_installation", s("yes"));
    apply(153, &mut state, Platform::Linux).unwrap();
    let jeff_ac = get(&ShippedData::get().expert_tunes, &["jeffweaver", "db", "ac", "max_telemetry_vars"]);
    assert_eq!(get(&state.profiles, &["ac", "max_telemetry_vars"]), jeff_ac);
    assert_eq!(get(&state.profiles, &["ac", "motion_boost_factor"]), &LegacyValue::Int(5));
    assert_eq!(get(&state.profiles, &["ac", "motion_effect_smoothing_factor", "yaw"]), &LegacyValue::Int(20));
    assert_eq!(
        get(&state.profiles, &["ac", "vehicles", "unpublished car", "motion_effect_smoothing_factor", "surge"]),
        &LegacyValue::Int(20)
    );
    assert!(
        get(&state.profiles, &["rbr"]).as_dict().unwrap().get("max_telemetry_vars").is_none(),
        "rbr not in jeff's set"
    );
    assert_eq!(get(&state.config, &["APP", "force_manual_quit"]), &s("no"));
}

/// v153 for an existing installation only adds `force_manual_quit` (compared with exactly `yes`).
#[test]
fn v153_existing_install_keeps_tunes() {
    let mut state = LegacyState { config: config(vec![]), profiles: profile_for_v153(), ..LegacyState::default() };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("first_time_installation", s("True"));
    apply(153, &mut state, Platform::Linux).unwrap();
    assert!(get(&state.profiles, &["ac"]).as_dict().unwrap().get("motion_boost_factor").is_none());
}

/// v238 sets belt smoothing 20 on games but 5 on vehicles.
#[test]
fn v238_game_and_vehicle_values_differ() {
    let mut state = LegacyState {
        config: config(vec![]),
        profiles: dict(vec![(
            "ac",
            tune_with(vec![("vehicles", LegacyValue::Dict(dict(vec![("car", tune_with(vec![]))])))]),
        )]),
        ..LegacyState::default()
    };
    apply(238, &mut state, Platform::Linux).unwrap();
    assert_eq!(get(&state.profiles, &["ac", "belt_surge_smoothing"]), &LegacyValue::Int(20));
    assert_eq!(get(&state.profiles, &["ac", "vehicles", "car", "belt_sway_smoothing"]), &LegacyValue::Int(5));
}

/// v328 copies the rear traction-loss settings to the front ones; the front activation threshold
/// is seeded from `motion_main_actuators_traction_loss`, not from `min_traction_loss_activation`.
#[test]
fn v328_front_traction_loss_family() {
    let group = |v: LegacyValue| LegacyValue::Dict(dict(vec![("traction_loss", v)]));
    let mut state = LegacyState {
        config: config(vec![("MOTIONBOX", Dict::new())]),
        profiles: dict(vec![(
            "ac",
            tune_with(vec![
                ("motion_main_actuators_traction_loss", 7.into()),
                ("min_traction_loss_activation", 30.into()),
                ("motion_effects", group(16.into())),
                ("motion_switch", group(true.into())),
                ("max_telemetry_vars", group(2.into())),
                ("motion_effect_smoothing_factor", group(20.into())),
            ]),
        )]),
        ..LegacyState::default()
    };
    apply(328, &mut state, Platform::Linux).unwrap();
    assert_eq!(get(&state.profiles, &["ac", "min_traction_loss_front_activation"]), &LegacyValue::Int(7));
    assert_eq!(get(&state.profiles, &["ac", "motion_effects", "traction_loss_front"]), &LegacyValue::Int(16));
    assert_eq!(get(&state.config, &["MOTIONBOX", "traction_loss_front_clip_factor"]), &LegacyValue::Int(1000));
}

/// v402: log level names become numbers; numeric values are kept.
#[test]
fn v402_log_level_names() {
    for (raw, expected) in [
        ("debug", s("10").py_int().map(LegacyValue::Int)),
        ("Error", Some(40.into())),
        ("verbose", Some(30.into())),
        ("20", None),
    ] {
        let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
        state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("log_level", s(raw));
        apply(402, &mut state, Platform::Linux).unwrap();
        let got = get(&state.config, &["APP", "log_level"]).clone();
        assert_eq!(got, expected.unwrap_or_else(|| s(raw)), "{raw}");
    }
    let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
    apply(402, &mut state, Platform::Linux).unwrap();
    assert_eq!(get(&state.config, &["APP", "log_level"]), &LegacyValue::Int(30), "missing → '30' → 30");
}

/// v412 renames the Thanos controller keys; an existing new key wins.
#[test]
fn v412_renames_controller_keys() {
    let mut state = LegacyState {
        config: config(vec![
            (
                "THANOS",
                dict(vec![
                    ("thanos_com_port", s("COM5")),
                    ("send_spike_filter", s("no")),
                    ("thanos_controller", s("AMC")),
                ]),
            ),
            ("PT-ACTUATOR", dict(vec![("thanos_com_port", s("COM6")), ("com_port", s("COM7"))])),
        ]),
        ..LegacyState::default()
    };
    apply(412, &mut state, Platform::Windows).unwrap();
    let thanos = get(&state.config, &["THANOS"]).as_dict().unwrap();
    assert_eq!(thanos.str_keys(), ["com_port", "filter"]);
    assert_eq!(thanos.get("com_port"), Some(&s("COM5")));
    assert_eq!(thanos.get("filter"), Some(&s("no")));
    let pt = get(&state.config, &["PT-ACTUATOR"]).as_dict().unwrap();
    assert_eq!(pt.get("com_port"), Some(&s("COM7")));
    assert_eq!(pt.get("filter"), Some(&s("yes")), "setdefault");
}

/// v168 creates a key with a trailing space and v174 drops it again; v316's typo key stays.
#[test]
fn shortcut_typo_keys() {
    let (state, _) = fresh();
    let shortcuts = get(&state.config, &["SHORTCUTS"]).as_dict().unwrap();
    assert!(!shortcuts.contains("motion_box_rumble_strips_up_btns "));
    assert!(shortcuts.contains("motion_box_rumble_strips_up_btns"));
    assert!(shortcuts.contains("ejection_seat_btns "));
    assert!(!shortcuts.contains("ejection_seat_btns"));
    assert!(!shortcuts.contains("cannon_firing_btns "), "fixed at v237");
}

/// Runs step 99 with the given `<docs>/profiles.json` content.
fn apply_v99(legacy: &LegacyLayouts) -> (LegacyState, Result<MigrationReport, String>) {
    let data = ShippedData::get();
    let sources = data.sources(Platform::Windows, legacy);
    let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.current = 99;
    let result = (STEPS.iter().find(|s| s.version == 99).unwrap().apply)(&mut m);
    let report = std::mem::take(&mut m.report);
    (state, result.map(|()| report))
}

/// Step v99 prefers `<docs>/profiles.json` over the shipped layouts and reports the
/// file deletion.
#[test]
fn v99_prefers_user_layout_file() {
    let legacy = LegacyLayouts::Layouts(dict(vec![("My layout", LegacyValue::Dict(Dict::new()))]));
    let (state, report) = apply_v99(&legacy);
    assert_eq!(report.unwrap().skipped_side_effects, ["v99: delete <docs>/profiles.json"]);
    assert_eq!(state.shaker_layouts.str_keys(), ["My layout"]);
}

/// Step v99: a user file that does not open or parse falls back to the shipped layouts
/// and is still deleted afterwards; without a user file
/// nothing is deleted.
#[test]
fn v99_falls_back_to_shipped_layouts() {
    let shipped = ShippedData::get().sources(Platform::Windows, &LegacyLayouts::Absent).shipped_layouts.str_keys();
    let (state, report) = apply_v99(&LegacyLayouts::Unreadable);
    assert_eq!(state.shaker_layouts.str_keys(), shipped);
    assert_eq!(report.unwrap().skipped_side_effects, ["v99: delete <docs>/profiles.json"]);
    let (state, report) = apply_v99(&LegacyLayouts::Absent);
    assert_eq!(state.shaker_layouts.str_keys(), shipped);
    assert!(report.unwrap().skipped_side_effects.is_empty());
}

/// Step v99: the fallback covers only reading the file, so valid JSON that
/// is not an object fails the step.
#[test]
fn v99_fails_on_json_that_is_not_an_object() {
    let (_, report) = apply_v99(&LegacyLayouts::NotAnObject("list"));
    assert_eq!(report.unwrap_err(), "'list' object has no attribute 'items'");
}

/// Step v123: the rebuilt metadata.db holds only `joystick` and `shakers`.
#[test]
fn v123_drops_notification_tables() {
    let marks = dict(vec![("ac", LegacyValue::Bool(true))]);
    let layouts = dict(vec![("My layout", LegacyValue::Dict(Dict::new()))]);
    let mut state = LegacyState {
        config: config(vec![]),
        shaker_layouts: layouts.clone(),
        joysticks: marks.clone(),
        game_config_notifications: marks.clone(),
        notifications: marks.clone(),
        ..LegacyState::default()
    };
    apply(123, &mut state, Platform::Linux).unwrap();
    assert!(state.game_config_notifications.is_empty());
    assert!(state.notifications.is_empty());
    assert_eq!(state.shaker_layouts, layouts);
    assert_eq!(state.joysticks, marks);
}

/// v265: a 30 Hz rate becomes the manufacturer default; Linux registers no PROSIMU manufacturer, so
/// the step fails there.
#[test]
fn v265_manufacturer_refresh_rate() {
    let state_for = |model: &str| LegacyState {
        config: config(vec![
            ("MOTIONBOX", dict(vec![("model", s(model)), ("refresh_rate", s("30"))])),
            ("CUSTOM_FEATURES", Dict::new()),
        ]),
        ..LegacyState::default()
    };
    let mut state = state_for("T1000-5M PRS200");
    apply(265, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["MOTIONBOX", "refresh_rate"]), &LegacyValue::Int(500));
    let mut state = state_for("P3");
    apply(265, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["MOTIONBOX", "refresh_rate"]), &s("30"), "DOF REALITY keeps 30");
    let mut state = state_for("unknown model");
    apply(265, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["MOTIONBOX", "refresh_rate"]), &s("30"), "unknown model counts as None");
    let mut state = state_for("T1000-5M PRS200");
    assert!(apply(265, &mut state, Platform::Linux).is_err());
}

/// Step v265: the model lookup's fallback
/// also covers a missing key or section, which count as model `None`; only the refresh-rate
/// conversion can still fail.
#[test]
fn v265_without_motion_model() {
    let mut state = LegacyState {
        config: config(vec![("MOTIONBOX", dict(vec![("refresh_rate", s("30"))])), ("CUSTOM_FEATURES", Dict::new())]),
        ..LegacyState::default()
    };
    apply(265, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["MOTIONBOX", "refresh_rate"]), &s("30"));
    assert_eq!(get(&state.config, &["CUSTOM_FEATURES", "enable_legacy_gear_smoothing"]), &s("no"));
    let listed = LegacyValue::List(vec![s("T1000-5M PRS200")]);
    let mut state = LegacyState {
        config: config(vec![
            ("MOTIONBOX", dict(vec![("model", listed), ("refresh_rate", s("30"))])),
            ("CUSTOM_FEATURES", Dict::new()),
        ]),
        ..LegacyState::default()
    };
    apply(265, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["MOTIONBOX", "refresh_rate"]), &s("30"), "unhashable model counts as None");
    let mut state = LegacyState { config: config(vec![("CUSTOM_FEATURES", Dict::new())]), ..LegacyState::default() };
    assert!(apply(265, &mut state, Platform::Windows).unwrap_err().contains("MOTIONBOX"), "int(refresh_rate) raises");
}

/// v354: only the untouched factory compensation values (sum 300) select the virtual sensor.
#[test]
fn v354_sensor_type() {
    let axis = |offset: &str| LegacyValue::Dict(dict(vec![("offset", s(offset)), ("reverse", s("False"))]));
    let state_for = |sway: &str| LegacyState {
        config: config(vec![(
            "MOTION_COMPENSATION",
            dict(vec![
                ("sway_factor", s(sway)),
                ("surge_factor", s("100")),
                ("surge_horizontal_factor", s("0")),
                ("surge_horizontal_stroke", s("100")),
                ("roll", axis("0")),
                ("pitch", axis("0.0")),
                ("surge", axis("0")),
            ]),
        )]),
        ..LegacyState::default()
    };
    let mut state = state_for("100");
    apply(354, &mut state, Platform::Linux).unwrap();
    assert_eq!(get(&state.config, &["MOTION_COMPENSATION", "type"]), &s("V"));
    assert_eq!(get(&state.config, &["MOTION_COMPENSATION", "surge", "reverse"]), &LegacyValue::Bool(true));
    let mut state = state_for("90");
    apply(354, &mut state, Platform::Linux).unwrap();
    assert_eq!(get(&state.config, &["MOTION_COMPENSATION", "type"]), &s("W"));
    assert_eq!(get(&state.config, &["MOTION_COMPENSATION", "surge", "reverse"]), &s("False"));
}

/// v230: non-Windows first installations get the magicbox hardware; Windows reads the registry
/// (not available, reported).
#[test]
fn v230_first_install_hardware() {
    let sections = || {
        config(vec![
            ("SHAKEBOX", dict(vec![("soundcard", s("None")), ("profile", s("x"))])),
            ("MOTIONBOX", dict(vec![("model", s("MP2")), ("refresh_rate", s("30"))])),
        ])
    };
    let mut state = LegacyState { config: sections(), ..LegacyState::default() };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("first_time_installation", s("yes"));
    apply(230, &mut state, Platform::Linux).unwrap();
    assert_eq!(get(&state.config, &["MOTIONBOX", "model"]), &s("H3"));
    assert_eq!(get(&state.config, &["MOTIONBOX", "refresh_rate"]), &LegacyValue::Int(30));
    assert_eq!(get(&state.config, &["SHAKEBOX", "profile"]), &s("SRS UShake6"));
    let mut state = LegacyState { config: sections(), ..LegacyState::default() };
    let report = apply(230, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["MOTIONBOX", "model"]), &s("MP2"));
    assert_eq!(report.skipped_side_effects.len(), 1);
    // Windows first installation: saved registry values are used.
    let mut state = LegacyState { config: sections(), ..LegacyState::default() };
    state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("first_time_installation", s("yes"));
    state.registry.insert("Soundcard", s("Speakers (USB)"));
    state.registry.insert("MotionModel", s("H3"));
    apply(230, &mut state, Platform::Windows).unwrap();
    assert_eq!(get(&state.config, &["SHAKEBOX", "soundcard"]), &s("Speakers (USB)"));
    assert_eq!(get(&state.config, &["MOTIONBOX", "model"]), &s("H3"));
    assert_eq!(get(&state.config, &["SHAKEBOX", "profile"]), &s("x"));
}

/// v288 moves `start_with_windows` into the Windows registry (`'1'` unless it reads false) and
/// sets `ShowSplashScreen`; v359 fills both when unset. Other platforms keep no registry.
#[test]
fn v288_v359_registry_switches() {
    let with_start = |value: &str| {
        let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
        state.config.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("start_with_windows", s(value));
        state
    };
    let mut state = with_start("no");
    apply(288, &mut state, Platform::Windows).unwrap();
    assert_eq!(state.registry.get("StartWithWindows"), Some(&s("0")));
    assert_eq!(state.registry.get("ShowSplashScreen"), Some(&s("1")));
    assert!(!state.config.get("APP").unwrap().as_dict().unwrap().contains("start_with_windows"));
    let mut state = with_start("maybe");
    apply(288, &mut state, Platform::Windows).unwrap();
    assert_eq!(state.registry.get("StartWithWindows"), Some(&s("1")), "str2bool raises: except branch");
    apply(359, &mut state, Platform::Windows).unwrap();
    assert_eq!(state.registry.get("StartWithWindows"), Some(&s("1")));
    let mut state = with_start("no");
    apply(288, &mut state, Platform::Linux).unwrap();
    apply(359, &mut state, Platform::Linux).unwrap();
    assert!(state.registry.is_empty());
    let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
    apply(359, &mut state, Platform::Windows).unwrap();
    assert_eq!(state.registry.get("StartWithWindows"), Some(&s("1")));
    assert_eq!(state.registry.get("ShowSplashScreen"), Some(&s("1")));
}

/// Shaker layouts after the chain: 9 layouts with 34 per-speaker effects each; UShake6 is the 5.1
/// layout with the first-install engine levels (v285 then v342).
#[test]
fn fresh_shaker_layouts() {
    let (state, _) = fresh();
    assert_eq!(state.shaker_layouts.len(), 9);
    let ushake =
        |channel: &str, effect: &str| get(&state.shaker_layouts, &["SRS UShake6", channel, "sfx", effect]).clone();
    assert_eq!(ushake("CE", "engine"), LegacyValue::Int(4));
    assert_eq!(ushake("SUB", "engine"), LegacyValue::Int(4));
    assert_eq!(
        ushake("FL", "abs_lock"),
        get(&state.shaker_layouts, &["6 Speakers - 5.1", "FL", "sfx", "brake"]).clone()
    );
    assert_eq!(
        get(&state.shaker_layouts, &["1 Speaker - Center", "CE", "sfx", "afterburner"]),
        &LegacyValue::Int(5),
        "flight levels from shakers_profile_config"
    );
    for (_, layout) in state.shaker_layouts.iter() {
        for (_, channel) in layout.as_dict().unwrap().iter() {
            assert_eq!(channel.as_dict().unwrap().get("sfx").unwrap().as_dict().unwrap().len(), 34);
        }
    }
    let default = get(&state.joysticks, &["Default", "front"]);
    assert_eq!(get(default.as_dict().unwrap(), &["trigger"]), &s("2"), "v340 replaces v325");
}

/// Flight shaker level lookup: 0 when no layout/effect row matches, the row's
/// channel value otherwise, an error when the matching row lacks the channel.
#[test]
fn flight_level_lookup() {
    let row = |effect: &str, fl: i64| LegacyValue::Dict(dict(vec![("effect", s(effect)), ("FL", fl.into())]));
    let table = dict(vec![("Layout", LegacyValue::List(vec![row("canopy", 7), row("canopy", 9)]))]);
    assert_eq!(flight_level(&table, "Layout", "canopy", "FL"), Ok(LegacyValue::Int(7)), "first matching row");
    assert_eq!(flight_level(&table, "Layout", "afterburner", "FL"), Ok(LegacyValue::Int(0)), "no row for the effect");
    assert_eq!(flight_level(&table, "Other", "canopy", "FL"), Ok(LegacyValue::Int(0)), "no such layout");
    assert_eq!(flight_level(&table, "Layout", "canopy", "SUB"), Err("KeyError: \"SUB\"".to_owned()));
    let no_effect = dict(vec![("Layout", LegacyValue::List(vec![LegacyValue::Dict(dict(vec![("FL", 1.into())]))]))]);
    assert_eq!(flight_level(&no_effect, "Layout", "canopy", "FL"), Err("KeyError: \"effect\"".to_owned()));
}

/// Step v214 at the start of the app: with the classic DCS `Export.lua` patched (the
/// library template with the `config.ini` port), it is removed and the DCS hook is patched in;
/// without a patch context the step is only reported.
#[test]
fn v214_repatches_dcs_at_start() {
    use crate::patch::{library, ModulePorts, PatchContext, WinePrefix};
    let tmp = tempfile::tempdir().unwrap();
    let prefix = WinePrefix::open(tmp.path().join("pfx"));
    let scripts = prefix.profile().join("Saved Games/DCS/Scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let template = String::from_utf8(library::file("Export.lua").unwrap().to_vec()).unwrap();
    std::fs::write(scripts.join("Export.lua"), template.replace("^port^", "41230")).unwrap();

    let mut state = LegacyState {
        config: config(vec![("NETWORK_PORT", dict(vec![("dcs", s("41230"))]))]),
        ..LegacyState::default()
    };
    let no_ports = |_| None;
    let loaded = ModulePorts::at_startup(&no_ports);
    let prefixes = [prefix];
    let context = PatchContext {
        prefixes: &prefixes,
        steam: None,
        library: &library::BUILTIN,
        ports: &no_ports,
        loaded_ports: &loaded,
    };
    let data = ShippedData::get();
    let docs = tmp.path().join("docs");
    let sources = MigrationSources {
        effects: Some(StartEffects { docs: &docs, patch: Some(&context) }),
        ..data.sources(Platform::Windows, &LegacyLayouts::Absent)
    };
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.current = 214;
    (STEPS.iter().find(|s| s.version == 214).unwrap().apply)(&mut m).unwrap();
    assert!(m.report.skipped_side_effects.is_empty());
    assert!(!scripts.join("Export.lua").exists());
    let hook = std::fs::read_to_string(scripts.join("Hooks/SRSExport.lua")).unwrap();
    assert!(hook.contains("41230"));

    let mut state = LegacyState { config: config(vec![]), ..LegacyState::default() };
    let report = apply(214, &mut state, Platform::Windows).unwrap();
    assert_eq!(report.skipped_side_effects, ["v214: re-patch DCS Export.lua"]);
}

#[test]
fn profile_copy_migration_reads_current_database_rows_instead_of_cached_values() {
    use crate::import::sqlitedict;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.db");
    let stale = tune_with(vec![("gear_shift_perc", 1.into())]);
    let live = tune_with(vec![("gear_shift_perc", 77.into())]);
    let mut state = LegacyState {
        config: config(vec![]),
        profiles: dict(vec![("projectcars2", stale.clone()), ("projectcars2udp", stale)]),
        ..LegacyState::default()
    };
    for source in ["projectcars2", "projectcars2udp"] {
        sqlitedict::set_item(&path, "unnamed", source, &live).unwrap();
    }
    let sources = MigrationSources {
        effects: Some(StartEffects { docs: directory.path(), patch: None }),
        ..ShippedData::get().sources(Platform::Linux, &LegacyLayouts::Absent)
    };
    let mut migrator = Migrator::new(&mut state, &sources).unwrap();
    migrator.current = 120;
    (STEPS.iter().find(|step| step.version == 120).unwrap().apply)(&mut migrator).unwrap();
    drop(migrator);
    for target in ["projectcars3", "projectcars3udp"] {
        assert_eq!(sqlitedict::read_item(&path, "unnamed", target).unwrap(), Some(live.clone()));
        assert_eq!(state.profiles.get(target), Some(&live));
    }
}

#[test]
fn tune_migration_decodes_rows_lazily_and_keeps_the_prefix_pending_on_bad_pickle() {
    use crate::import::{pickle, sqlitedict};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.db");
    let original = tune_with(vec![("gear_shift_perc", 1.into())]);
    let bytes = pickle::encode(&original);
    sqlitedict::tests::write_table(&path, "unnamed", &[("first", &bytes), ("bad", &[255]), ("last", &bytes)]);
    // This typed snapshot deliberately has only valid rows. Runtime
    // iteration must still encounter and reject the actual late database row.
    let mut state = LegacyState {
        config: config(vec![]),
        profiles: dict(vec![("first", original.clone()), ("last", original.clone())]),
        ..LegacyState::default()
    };
    let sources = MigrationSources {
        effects: Some(StartEffects { docs: directory.path(), patch: None }),
        ..ShippedData::get().sources(Platform::Linux, &LegacyLayouts::Absent)
    };
    let mut migrator = Migrator::new(&mut state, &sources).unwrap();
    migrator.current = 129;
    let error = (STEPS.iter().find(|step| step.version == 129).unwrap().apply)(&mut migrator).unwrap_err();
    assert!(error.contains("bad"), "{error}");
    let pending = migrator.profile_db.as_ref().unwrap();
    let first = pending.get("first").unwrap().unwrap();
    assert_ne!(first, original);
    assert_eq!(pending.get("last").unwrap(), Some(original.clone()));
    assert_eq!(sqlitedict::read_item(&path, "unnamed", "first").unwrap(), Some(original.clone()));
    pending.close().unwrap();
    assert_eq!(sqlitedict::read_item(&path, "unnamed", "first").unwrap(), Some(original));
}

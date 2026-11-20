//! Shared-object (aliased) value graphs that cannot be represented by borrowed tree dictionaries.
use super::*;
use crate::import::value::{NativeKey, NativeObject};
use crate::import::{shipped::ShippedData, sqlitedict};

fn dict(entries: &[(&str, LegacyValue)]) -> LegacyValue {
    LegacyValue::Dict(entries.iter().map(|(k, v)| (Key::from(*k), v.clone())).collect())
}
fn shared(entries: &[(&str, LegacyValue)]) -> LegacyValue {
    LegacyValue::Object(NativeObject::new(dict(entries)))
}
fn state(root: LegacyValue) -> LegacyState {
    LegacyState {
        config: [(
            Key::from("APP"),
            dict(&[("config_file_version", "412".into()), ("first_time_installation", "yes".into())]),
        )]
        .into_iter()
        .collect(),
        profiles: [(Key::from("game"), root)].into_iter().collect(),
        ..Default::default()
    }
}
fn run(version: u32, state: &mut LegacyState) -> StepResult {
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    let mut m = Migrator::new(state, &sources).map_err(|e| e.message)?;
    m.current = version;
    (STEPS.iter().find(|s| s.version == version).unwrap().apply)(&mut m)
}
fn at(root: &LegacyValue, path: &[&str]) -> LegacyValue {
    path.iter().fold(root.clone(), |v, k| v.item_owned(k).unwrap())
}

#[test]
fn shared_vehicle_is_visited_once_per_name_in_source_order() {
    let vehicle = shared(&[("visits", 0.into())]);
    let root = shared(&[("vehicles", dict(&[("first", vehicle.clone()), ("second", vehicle.clone())]))]);
    let mut state = state(root);
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    let mut levels = Vec::new();
    m.each_tune(|_, tune, level| {
        levels.push(level);
        if level == Level::Vehicle {
            let n = tune.get("visits").unwrap().py_add(&1.into())?;
            tune.insert("visits", n)?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(levels, [Level::Game, Level::Vehicle, Level::Vehicle]);
    assert_eq!(at(&vehicle, &["visits"]), 2.into());
    let root = m.state.profiles.get("game").unwrap();
    assert!(at(root, &["vehicles", "first"]).same_object(&at(root, &["vehicles", "second"])));
}

#[test]
fn source_v238_self_vehicle_overwrites_the_game_value_without_deadlock() {
    let mut root = shared(&[("vehicles", dict(&[]))]);
    root.assign_path(&["vehicles", "self"], root.clone()).unwrap();
    let mut state = state(root.clone());
    run(238, &mut state).unwrap();
    assert_eq!(at(&root, &["belt_surge_smoothing"]), 5.into());
    assert_eq!(at(&root, &["belt_sway_smoothing"]), 5.into());
    assert!(at(&root, &["vehicles", "self"]).same_object(&root));
}

#[test]
fn source_v205_reuses_one_nested_group_identity_for_all_assignments() {
    let group = shared(&[]);
    let root = shared(&[
        ("vehicles", dict(&[])),
        ("shaker_effects", group.clone()),
        ("shaker_dsp_type", group.clone()),
        ("shaker_cutoff", group.clone()),
        ("shaker_resonance", group.clone()),
        ("shaker_switch", group.clone()),
    ]);
    let mut state = state(root.clone());
    run(205, &mut state).unwrap();
    assert_eq!(at(&group, &["cannon_firing"]), true.into());
    for name in ["shaker_effects", "shaker_dsp_type", "shaker_cutoff", "shaker_resonance", "shaker_switch"] {
        assert!(at(&root, &[name]).same_object(&group));
    }
}

#[test]
fn source_nested_failure_keeps_the_prior_shared_statement_visible() {
    let group = shared(&[]);
    let root = shared(&[("vehicles", dict(&[])), ("shaker_effects", group.clone()), ("shaker_dsp_type", 0.into())]);
    let mut state = state(root);
    let error = run(205, &mut state).unwrap_err();
    assert!(error.contains("int") && error.contains("item assignment"), "{error}");
    assert_eq!(at(&group, &["afterburner"]), 10.into());
}

#[test]
fn layout_channels_observe_the_same_live_effects_and_partial_failure() {
    let sfx = shared(&[("visits", 0.into())]);
    let channel = shared(&[("sfx", sfx.clone())]);
    let layout = shared(&[("FL", channel.clone()), ("FR", channel), ("bad", 0.into())]);
    let mut state = state(dict(&[]));
    state.shaker_layouts.insert("layout", layout);
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    let mut visited = Vec::new();
    let error = m
        .each_layout_channel_named(|_, name, sfx| {
            visited.push(name.to_owned());
            sfx.insert("visits", sfx.get_result("visits")?.py_add(&1.into())?)?;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(visited, ["FL", "FR", "bad"]);
    assert!(error.contains("int") && error.contains("subscriptable"), "{error}");
    assert_eq!(at(&sfx, &["visits"]), 2.into());
}

#[test]
fn source_copy_section_detaches_only_the_root_at_its_copy_statement() {
    let child = shared(&[("value", 1.into())]);
    let source = shared(&[("child", child.clone())]);
    let mut state = state(dict(&[]));
    state.config.insert("source", source.clone());
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.copy_section("source", "copy").unwrap();
    let mut copy = m.section("copy").unwrap();
    nested(&mut copy, "child").unwrap().insert("value", 2.into()).unwrap();
    copy.insert("new", true.into()).unwrap();
    assert_eq!(at(&child, &["value"]), 2.into());
    assert!(source.item_owned("new").is_err());
}

#[test]
fn source_v123_shallow_copy_retains_the_old_self_identity() {
    let mut root = shared(&[("vehicles", dict(&[]))]);
    root.assign_path(&["vehicles", "self"], root.clone()).unwrap();
    let mut state = state(root.clone());
    run(123, &mut state).unwrap();
    let rebuilt = state.profiles.get("game").unwrap();
    assert!(!rebuilt.same_object(&root));
    assert!(at(rebuilt, &["vehicles", "self"]).same_object(&root));
    assert_eq!(at(&root, &["vehicles"]).py_len(), Some(0));
}

#[test]
fn source_key_iteration_rejects_size_change_after_the_completed_child() {
    let mut vehicles = shared(&[]);
    vehicles.assign_item("self", vehicles.clone()).unwrap();
    let root = shared(&[("vehicles", vehicles.clone())]);
    let mut state = state(root);
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    let error = m
        .each_tune(|_, tune, level| {
            if level == Level::Vehicle {
                tune.insert("added", true.into())?;
            }
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error, "RuntimeError: dictionary changed size during iteration");
    assert_eq!(at(&vehicles, &["added"]), true.into());
}

#[test]
fn source_iteration_does_not_silently_exhaust_after_equal_size_key_replacement() {
    let child = shared(&[]);
    let mut vehicles = shared(&[("a", child.clone()), ("b", child.clone())]);
    let root = shared(&[("vehicles", vehicles.clone())]);
    let mut state = state(root);
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    let mut visits = 0;
    let error = m
        .each_tune(|_, _, level| {
            if level == Level::Vehicle {
                visits += 1;
                if visits == 1 {
                    vehicles.remove_key(&"a".into())?;
                    vehicles.assign_item("c", child.clone())?;
                }
            }
            Ok(())
        })
        .unwrap_err();
    assert_eq!(visits, 2);
    assert_eq!(error, "RuntimeError: dictionary keys changed during iteration");
}

#[test]
fn source_rekey_preserves_surrogates_and_memoized_key_identity() {
    let name = LegacyValue::from_python_string(PythonString::from_codepoints(vec![0xd800, 65, 0, 0]).unwrap());
    let key = NativeObject::new(name.clone());
    let vehicle = shared(&[("vehicles", dict(&[]))]);
    let mut vehicles = shared(&[]);
    vehicles.assign_key(LegacyValue::Object(key.clone()), vehicle.clone()).unwrap();
    let root = shared(&[("vehicles", vehicles.clone())]);
    let mut state = state(dict(&[]));
    state.profiles = Dict::new();
    state.profiles.insert("fs2020", root);
    run(222, &mut state).unwrap();
    let keys = vehicles.dict_keys_owned().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].to_value().python_string().unwrap().codepoints(), &[0xd800, 65]);
    assert!(vehicles.lookup_owned(&keys[0].to_value()).unwrap().same_object(&vehicle));
    assert_eq!(LegacyValue::Object(key).python_string(), name.python_string());
    assert!(!matches!(keys[0], Key::Extra(NativeKey::Object(_))), "new substring is a new string object");
}

#[test]
fn source_native_scalars_preserve_unbounded_arithmetic_and_version() {
    let huge = BigInt::from(1u32) << 100usize;
    let version = huge.clone();
    let mut state = state(dict(&[]));
    state.config.insert(
        "APP",
        shared(&[
            ("config_file_version", LegacyValue::from_integer(version.clone())),
            ("first_time_installation", shared_scalar("no".into())),
        ]),
    );
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    let report = super::super::migrate(&mut state, &sources).unwrap();
    assert_eq!(report.from_version, version);
    assert!(report.applied.is_empty());
    assert_eq!(
        state.config.get("APP").unwrap().item_owned("config_file_version").unwrap().integer(),
        Some(huge.clone())
    );
    state.config.insert("INTELLIBOX", shared(&[]));
    state.profiles.insert(
        "emulator",
        shared(&[
            ("wind_type", "Fan".into()),
            ("wind_curving", shared_scalar(LegacyValue::from_integer(huge.clone()))),
        ]),
    );
    run(88, &mut state).unwrap();
    assert_eq!(state.config.get("INTELLIBOX").unwrap().item_owned("nbr_fans").unwrap().integer(), Some(huge + 1));
}
fn shared_scalar(value: LegacyValue) -> LegacyValue {
    LegacyValue::Object(NativeObject::new(value))
}

#[test]
fn source_config_aliases_and_missing_local_keep_assignment_order() {
    let app = shared(&[("config_file_version", "412".into())]);
    let mut state = state(dict(&[]));
    state.config.insert("APP", app.clone());
    state.config.insert("observer", app.clone());
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Linux, &absent);
    assert!(Migrator::new(&mut state, &sources).err().unwrap().message.contains("first_time_installation"));
    assert_eq!(at(&app, &["first_time_installation"]), "no".into());
    state.config.insert("THANOS", shared(&[("thanos_com_port", app.clone())]));
    state.config.insert("PT-ACTUATOR", shared(&[]));
    run(412, &mut state).unwrap();
    assert!(state.config.get("THANOS").unwrap().item_owned("com_port").unwrap().same_object(&app));
}

#[test]
fn actual_database_roundtrip_preserves_a_self_vehicle() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.db");
    let mut root = shared(&[("vehicles", dict(&[]))]);
    root.assign_path(&["vehicles", "self"], root.clone()).unwrap();
    sqlitedict::set_item(&path, "unnamed", "game", &root).unwrap();
    let mut state = state(dict(&[]));
    let absent = LegacyLayouts::Absent;
    let sources = MigrationSources {
        effects: Some(super::super::StartEffects { docs: directory.path(), patch: None }),
        ..ShippedData::get().sources(Platform::Linux, &absent)
    };
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.current = 238;
    v238(&mut m).unwrap();
    let saved = sqlitedict::read_item(&path, "unnamed", "game").unwrap().unwrap();
    assert_eq!(at(&saved, &["belt_surge_smoothing"]), 5.into());
    assert!(at(&saved, &["vehicles", "self"]).same_object(&saved));
}

#[test]
fn integer_sum_and_shared_compensation_axes_use_native_source_order() {
    let huge = BigInt::from(1u32) << 100usize;
    let axis = shared(&[("offset", 0.0.into())]);
    let compensation = shared(&[
        ("sway_factor", LegacyValue::from_integer(huge.clone())),
        ("surge_factor", 100.into()),
        ("surge_horizontal_factor", LegacyValue::from_integer(-huge.clone())),
        ("surge_horizontal_stroke", 200.into()),
        ("roll", axis.clone()),
        ("pitch", axis.clone()),
        ("surge", axis.clone()),
    ]);
    let mut state = state(shared(&[("vehicles", dict(&[]))]));
    state.config.insert("MOTION_COMPENSATION", compensation.clone());
    run(354, &mut state).unwrap();
    assert_eq!(at(&compensation, &["type"]), "V".into());
    assert_eq!(at(&axis, &["reverse"]), true.into());
    state.config.insert("MOTIONBOX", shared(&[("reaction_speed", LegacyValue::from_integer(huge.clone()))]));
    run(366, &mut state).unwrap();
    assert_eq!(
        state.profiles.get("game").unwrap().item_owned("motion_reaction_speed").unwrap().integer(),
        Some(huge.clone())
    );
    let app = state.config.get_mut("APP").unwrap();
    app.assign_item("log_level", LegacyValue::from_integer(huge.clone())).unwrap();
    run(402, &mut state).unwrap();
    assert_eq!(state.config.get("APP").unwrap().item_owned("log_level").unwrap().integer(), Some(huge));
}

#[test]
fn actual_database_failure_keeps_shared_row_edits_uncommitted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profile.db");
    let group = shared(&[]);
    let complete = shared(&[
        ("vehicles", dict(&[])),
        ("shaker_effects", group.clone()),
        ("shaker_dsp_type", group.clone()),
        ("shaker_cutoff", group.clone()),
        ("shaker_resonance", group.clone()),
        ("shaker_switch", group),
    ]);
    let bad = shared(&[("vehicles", dict(&[])), ("shaker_effects", shared(&[])), ("shaker_dsp_type", 0.into())]);
    sqlitedict::set_item(&path, "unnamed", "first", &complete).unwrap();
    sqlitedict::set_item(&path, "unnamed", "bad", &bad).unwrap();
    let mut state = state(dict(&[]));
    let absent = LegacyLayouts::Absent;
    let sources = MigrationSources {
        effects: Some(super::super::StartEffects { docs: directory.path(), patch: None }),
        ..ShippedData::get().sources(Platform::Linux, &absent)
    };
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.current = 205;
    assert!(v205(&mut m).unwrap_err().contains("item assignment"));
    let pending = m.profile_db.as_ref().unwrap();
    let first = pending.get("first").unwrap().unwrap();
    assert_eq!(at(&first, &["shaker_switch", "cannon_firing"]), true.into());
    assert_eq!(pending.get("bad").unwrap().unwrap(), bad);
    assert_eq!(sqlitedict::read_item(&path, "unnamed", "first").unwrap().unwrap(), complete);
    assert_eq!(sqlitedict::read_item(&path, "unnamed", "bad").unwrap().unwrap(), bad);
}

#[test]
fn cyclic_vehicle_rebinds_the_full_target_before_the_next_statement() {
    let mut vehicles = shared(&[]);
    vehicles.assign_item("belt_surge_smoothing", vehicles.clone()).unwrap();
    let root = shared(&[("vehicles", vehicles.clone())]);
    let mut state = state(root);
    let error = run(238, &mut state).unwrap_err();
    assert!(error.contains("int") && error.contains("item assignment"), "{error}");
    assert_eq!(at(&vehicles, &["belt_surge_smoothing"]), 5.into());
    assert!(vehicles.item_owned("belt_sway_smoothing").is_err(), "source fails before this next assignment");
}

#[test]
fn nested_group_rebinds_the_receiver_before_its_next_source_assignment() {
    let mut vehicles = shared(&[]);
    let group = shared(&[("surge", 7.into())]);
    let vehicle = shared(&[("max_telemetry_vars", group)]);
    vehicles.assign_item("surge_acceleration", vehicle.clone()).unwrap();
    // First split assignment replaces the current vehicle through an alias.
    vehicle.clone().assign_item("max_telemetry_vars", vehicles.clone()).unwrap();
    vehicles.assign_item("surge", 7.into()).unwrap();
    let root = shared(&[("vehicles", vehicles.clone()), ("max_telemetry_vars", dict(&[]))]);
    let mut state = state(root);
    // v262 ignores the failure of each of its three parts separately; a subsequent numeric
    // vehicle is likewise skipped, rather than rejected by a borrowed view.
    run(262, &mut state).unwrap();
    assert_eq!(at(&vehicles, &["surge_acceleration"]), 7.into());
    assert!(vehicles.item_owned("surge_deceleration").is_err());
    assert_eq!(at(&vehicles, &["surge"]), 7.into());
}

#[test]
fn source_v230_assigns_the_native_registry_objects_without_detaching_them() {
    let card = shared_scalar("Speakers (USB)".into());
    let model = shared_scalar("H3".into());
    let mut state = state(dict(&[]));
    state.config.insert("SHAKEBOX", shared(&[("soundcard", "None".into())]));
    state.config.insert("MOTIONBOX", shared(&[("model", "MP2".into()), ("refresh_rate", 90.into())]));
    state.registry.insert("Soundcard", card.clone());
    state.registry.insert("MotionModel", model.clone());
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Windows, &absent);
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.current = 230;
    v230(&mut m).unwrap();
    assert!(at(m.state.config.get("SHAKEBOX").unwrap(), &["soundcard"]).same_object(&card));
    assert!(at(m.state.config.get("MOTIONBOX").unwrap(), &["model"]).same_object(&model));
    assert_eq!(at(m.state.config.get("MOTIONBOX").unwrap(), &["refresh_rate"]), 30.into());
}

#[test]
fn source_v230_caught_card_assignment_stops_before_the_model_assignment() {
    let model = shared_scalar("H3".into());
    let mut state = state(dict(&[]));
    state.config.insert("MOTIONBOX", shared(&[("model", "MP2".into()), ("refresh_rate", 90.into())]));
    state.registry.insert("Soundcard", "Speakers (USB)".into());
    state.registry.insert("MotionModel", model);
    let absent = LegacyLayouts::Absent;
    let sources = ShippedData::get().sources(Platform::Windows, &absent);
    let mut m = Migrator::new(&mut state, &sources).unwrap();
    m.current = 230;
    v230(&mut m).unwrap();
    assert_eq!(at(m.state.config.get("MOTIONBOX").unwrap(), &["model"]), "MP2".into());
    assert_eq!(at(m.state.config.get("MOTIONBOX").unwrap(), &["refresh_rate"]), 90.into());
}

use super::*;
use crate::import::configobj;
use crate::import::migrate::{migrate, LegacyLayouts, Platform};
use crate::import::shipped::ShippedData;
use crate::import::value::{parse_py_float, parse_py_int, str2bool};
use crate::profiles::ProfileStore;
use crate::tunes::ShakerLayouts;
use std::collections::BTreeMap;

#[test]
fn typed_integer_ui_settings_write_integral_native_text() {
    let settings = Settings::default();
    let raw = settings.native_snapshot();
    for (section, keys) in [
        ("CUSTOM_FEATURES", &["smoothing_slider_step"][..]),
        (
            "MOTION_COMPENSATION",
            &[
                "height",
                "surge_horizontal_stroke",
                "mm_distance_left_right",
                "mm_distance_front_back",
                "vertical_actuators_stroke",
                "surge_factor",
                "surge_horizontal_factor",
                "sway_factor",
                "yaw_factor",
                "roll_factor",
                "pitch_factor",
            ][..],
        ),
        ("BELT_TENSIONER", &["belt_calibration_factor", "belt_calibration_retraction"][..]),
    ] {
        for key in keys {
            let value = raw.get(section).unwrap().item(key).unwrap();
            assert!(value.as_str().and_then(parse_py_int).is_some(), "{section}.{key}: {value:?}");
        }
    }
    for (input, expected) in [
        (serde_json::json!(-0.0), "0"),
        (serde_json::json!(9007199254740993_i64), "9007199254740993"),
        (serde_json::json!(1.5), "1.5"),
        (serde_json::json!("1.0"), "1.0"),
    ] {
        let edited = settings.apply_ui_patch(&serde_json::json!({"features":{"smoothing_slider_step":input}})).unwrap();
        assert_eq!(
            edited.native_snapshot().get("CUSTOM_FEATURES").unwrap().item("smoothing_slider_step").unwrap(),
            &LegacyValue::from(expected)
        );
    }
    let original = configobj::parse("[CUSTOM_FEATURES]\nsmoothing_slider_step = 1.0\n").unwrap();
    let retained = Settings::from_legacy(&original).0;
    assert_eq!(retained.native_snapshot(), original, "native text is not a typed edit");
}

fn old_typed_integer_fixture(dir: &paths::ConfigDir) -> Vec<u8> {
    let config = Configuration::first_run();
    config.save(dir).unwrap();
    config.settings.save(&dir.old_settings_file()).unwrap();
    let text = std::fs::read_to_string(dir.settings_file()).unwrap();
    let text = text.replace("smoothing_slider_step = 1\n", "smoothing_slider_step = 1.0\n");
    std::fs::write(dir.settings_file(), text.as_bytes()).unwrap();
    std::fs::File::open(dir.old_settings_file())
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1)),
        )
        .unwrap();
    text.into_bytes()
}

#[test]
fn legacy_typed_integer_recovery_requires_exact_value_and_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temp.path());
    let original = old_typed_integer_fixture(&dir);
    let mut settings = Settings::load_ini_only(&dir.settings_file()).unwrap();
    assert!(settings.recover_legacy_typed_integers(&dir.settings_file(), &dir.old_settings_file()));
    assert_eq!(
        settings.native_snapshot().get("CUSTOM_FEATURES").unwrap().item("smoothing_slider_step").unwrap(),
        &LegacyValue::from("1")
    );
    assert_eq!(std::fs::read(dir.settings_file()).unwrap(), original, "recovery helper is read-only");
    std::fs::write(dir.old_settings_file(), "version = 1\n[features]\nsmoothing_slider_step = 1.0\n[shortcuts]\nbindings = 17\n[leds]\nsegments = 'invalid'\n").unwrap();
    std::fs::File::open(dir.old_settings_file())
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
        .unwrap();
    let mut unrelated = Settings::load_ini_only(&dir.settings_file()).unwrap();
    assert!(unrelated.recover_legacy_typed_integers(&dir.settings_file(), &dir.old_settings_file()));
    settings.edit_native(|raw| {
        raw.get_mut("CUSTOM_FEATURES").unwrap().as_dict_mut().unwrap().insert("smoothing_slider_step", "2.0".into());
    });
    assert!(!settings.recover_legacy_typed_integers(&dir.settings_file(), &dir.old_settings_file()));
    for old in [
        "version = 2\n[features]\nsmoothing_slider_step = 1.0\n",
        "version = 1\n[features]\nsmoothing_slider_step = 1\n",
        "version = 1\n[features]\n",
    ] {
        std::fs::write(dir.old_settings_file(), old).unwrap();
        std::fs::File::open(dir.old_settings_file())
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let mut settings = Settings::load_ini_only(&dir.settings_file()).unwrap();
        assert!(!settings.recover_legacy_typed_integers(&dir.settings_file(), &dir.old_settings_file()), "{old}");
    }
    std::fs::write(dir.old_settings_file(), "version = 1\n[features]\nsmoothing_slider_step = 1.0\n").unwrap();
    std::fs::File::open(dir.old_settings_file())
        .unwrap()
        .set_times(
            std::fs::FileTimes::new().set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)),
        )
        .unwrap();
    let mut settings = Settings::load_ini_only(&dir.settings_file()).unwrap();
    assert!(
        !settings.recover_legacy_typed_integers(&dir.settings_file(), &dir.old_settings_file()),
        "newer TOML cannot identify old output"
    );
}

#[test]
fn legacy_typed_integer_start_preserves_backup_and_other_ini_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temp.path());
    let original = old_typed_integer_fixture(&dir);
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(opened.reset.is_none());
    let backup = dir.root().join("config.ini.before-typed-integer-recovery");
    assert_eq!(std::fs::read(&backup).unwrap(), original);
    let expected =
        String::from_utf8(original).unwrap().replace("smoothing_slider_step = 1.0\n", "smoothing_slider_step = 1\n");
    assert_eq!(std::fs::read_to_string(dir.settings_file()).unwrap(), expected);
    std::fs::write(&backup, b"earlier recovery backup").unwrap();
    Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), b"earlier recovery backup");
}

#[test]
fn legacy_typed_integer_backup_failure_never_enters_windows_reset() {
    let temp = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temp.path());
    let original = old_typed_integer_fixture(&dir);
    let profiles = std::fs::read(dir.profiles_file()).unwrap();
    let metadata = std::fs::read(dir.metadata_file()).unwrap();
    std::fs::create_dir(dir.root().join("config.ini.before-typed-integer-recovery")).unwrap();
    assert!(Configuration::open_for(&dir, Platform::Windows).is_err());
    assert_eq!(std::fs::read(dir.settings_file()).unwrap(), original);
    assert_eq!(std::fs::read(dir.profiles_file()).unwrap(), profiles);
    assert_eq!(std::fs::read(dir.metadata_file()).unwrap(), metadata);
    assert!(!dir.root().join(RESET_FILE).exists());
}

#[test]
fn legacy_typed_integer_preflight_leaves_unrelated_parse_errors_to_startup() {
    let temp = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temp.path());
    std::fs::write(dir.settings_file(), b"[APP\ninvalid = [\n").unwrap();
    configuration::preserve_typed_integer_recovery_input(&dir).unwrap();
    std::fs::write(dir.old_settings_file(), b"version = 1\n[features]\nsmoothing_slider_step = 1.0\n").unwrap();
    configuration::preserve_typed_integer_recovery_input(&dir).unwrap();
    assert!(!dir.root().join("config.ini.before-typed-integer-recovery").exists());
}

#[test]
fn runtime_start_keeps_the_migration_dictionaries_for_its_callers() {
    let temporary = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temporary.path());
    let (opened, databases) = Configuration::open_runtime_session(&dir, Platform::Linux, None).unwrap();
    assert!(opened.reset.is_none());
    std::fs::remove_file(dir.profiles_file()).unwrap();
    std::fs::remove_file(dir.metadata_file()).unwrap();
    assert!(databases.profile.as_ref().unwrap().get("emulator").unwrap().is_some());
    assert!(databases.metadata["shakers"]
        .get(&opened.configuration.settings.shakers.speaker_layout)
        .unwrap()
        .is_some());
    for table in ["joystick", "shakers", "game_config_notifications", "notifications", "cache"] {
        assert!(databases.metadata[table].get("absent").unwrap().is_none());
    }
    assert!(!dir.profiles_file().exists());
    assert!(!dir.metadata_file().exists());
}

#[test]
fn older_runtime_ini_failure_precedes_final_notification_and_cache_constructors() {
    use crate::import::sqlitedict;
    let (_temporary, dir) = stored(Platform::Linux);
    let mut raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
    raw.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", "411".into());
    Settings::from_legacy(&raw).0.write_ini_only(&dir.settings_file()).unwrap();
    rusqlite::Connection::open(dir.metadata_file())
        .unwrap()
        .execute_batch("DROP TABLE notifications; DROP TABLE cache;")
        .unwrap();
    let mut permissions = std::fs::metadata(dir.settings_file()).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(dir.settings_file(), permissions).unwrap();
    assert!(Configuration::open_for_runtime(&dir, Platform::Linux, None).is_err());
    let tables = sqlitedict::table_names(&dir.metadata_file()).unwrap();
    assert!(!tables.iter().any(|name| name == "notifications" || name == "cache"));
}

#[test]
fn runtime_first_install_does_not_retry_caught_registry_failures_during_ini_write() {
    use crate::import::sqlitedict;
    let temporary = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temporary.path());
    // Registry write failure is independent of migration metadata replacement.
    // The registry setter's handled failure must not be retried by INI writing.
    let pending = dir.registry_file();
    sqlitedict::ensure_table(&pending, "registry").unwrap();
    rusqlite::Connection::open(&pending).unwrap().execute_batch(
        "CREATE TRIGGER reject_splash BEFORE INSERT ON registry WHEN NEW.key='ShowSplashScreen' BEGIN SELECT RAISE(ABORT,'splash flag write rejected'); END;"
    ).unwrap();
    let opened = Configuration::open_for_runtime(&dir, Platform::Windows, None).unwrap();
    assert!(opened.reset.is_none(), "caught registry failure must not reset the configuration: {:?}", opened.reset);
    assert_eq!(sqlitedict::read_item(&dir.registry_file(), "registry", "StartWithWindows").unwrap(), Some("1".into()));
    assert_eq!(sqlitedict::read_item(&dir.registry_file(), "registry", "ShowSplashScreen").unwrap(), None);
    assert!(!dir.root().join("reset.txt").exists());
    let raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
    assert_eq!(
        raw.get("APP").unwrap().as_dict().unwrap().get("config_file_version").unwrap().py_int(),
        Some(i64::from(crate::import::migrate::CURRENT_VERSION))
    );
}

#[test]
fn runtime_start_retains_successful_registry_migration_values() {
    use crate::import::sqlitedict;
    for version in [287, 358] {
        let (_temporary, dir) = stored(Platform::Windows);
        let mut raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
        let app = raw.get_mut("APP").unwrap().as_dict_mut().unwrap();
        app.insert("config_file_version", LegacyValue::from(version.to_string()));
        app.insert("start_with_windows", LegacyValue::from("no"));
        if version == 287 {
            app.insert("default_browser", LegacyValue::from("yes"));
        }
        // These fields still lived in PT-ACTUATOR before v391. Restore the
        // older schema's inputs rather than merely relabel the current INI.
        let thanos = raw.get("THANOS").unwrap().as_dict().unwrap().clone();
        let actuator = raw.get_mut("PT-ACTUATOR").unwrap().as_dict_mut().unwrap();
        for key in [
            "vertical_actuators_stroke",
            "thanos_com_port",
            "send_spike_filter",
            "reverse_controller_port_5",
            "reverse_controller_port_6",
            "reverse_controller_port_7",
            "reverse_controller_port_8",
        ] {
            let current = match key {
                "thanos_com_port" => "com_port",
                "send_spike_filter" => "filter",
                _ => key,
            };
            actuator.insert(key, thanos.get(current).unwrap().clone());
        }
        Settings::from_legacy(&raw).0.write_ini_only(&dir.settings_file()).unwrap();
        for (key, value) in [("StartWithWindows", if version == 287 { "1" } else { "0" }), ("ShowSplashScreen", "0")] {
            sqlitedict::set_item(&dir.registry_file(), "registry", key, &LegacyValue::from(value)).unwrap();
        }
        let before = std::fs::read(dir.metadata_file()).unwrap();
        let opened = Configuration::open_for_runtime(&dir, Platform::Windows, None).unwrap();
        assert!(opened.reset.is_none(), "schema {version}: {:?}", opened.reset);
        assert!(!opened.configuration.settings.app.start_with_login);
        assert_eq!(opened.configuration.settings.app.show_splash_screen, version == 287);
        assert_eq!(
            sqlitedict::read_item(&dir.registry_file(), "registry", "StartWithWindows").unwrap(),
            Some("0".into())
        );
        assert_eq!(
            sqlitedict::read_item(&dir.registry_file(), "registry", "ShowSplashScreen").unwrap(),
            Some(if version == 287 { "1" } else { "0" }.into())
        );
        if version == 358 {
            assert_eq!(std::fs::read(dir.metadata_file()).unwrap(), before);
        }
    }
}

#[test]
fn runtime_previous_schema_defers_unrelated_values_until_native_consumers_read_them() {
    use crate::import::sqlitedict;
    for platform in [Platform::Linux, Platform::Windows] {
        for table in ["profile", "joystick", "notifications", "shakers"] {
            let (_temporary, dir) = stored(platform);
            let mut raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
            raw.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", "411".into());
            Settings::from_legacy(&raw).0.save(&dir.settings_file()).unwrap();
            let path = if table == "profile" { dir.profiles_file() } else { dir.metadata_file() };
            let sql_table = if table == "profile" { "unnamed" } else { table };
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute(&format!("INSERT INTO {sql_table} (key,value) VALUES ('Opaque',?)"), [vec![255_u8]])
                .unwrap();
            let before = [std::fs::read(dir.profiles_file()).unwrap(), std::fs::read(dir.metadata_file()).unwrap()];
            let opened = Configuration::open_for_runtime(&dir, platform, None).unwrap();
            assert!(opened.reset.is_none());
            assert_eq!(
                [std::fs::read(dir.profiles_file()).unwrap(), std::fs::read(dir.metadata_file()).unwrap()],
                before,
                "{table}"
            );
            assert!(sqlitedict::read_item(&path, sql_table, "Opaque").is_err(), "opaque value remains in place");
        }
    }
}

#[test]
fn runtime_previous_schema_keeps_selected_none_layout_until_its_actual_consumer() {
    use crate::import::{sqlitedict, value::LegacyValue};
    let (_temporary, dir) = stored(Platform::Linux);
    let mut raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
    raw.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", "411".into());
    let settings = Settings::from_legacy(&raw).0;
    let name = settings.shakers.speaker_layout.clone();
    settings.save(&dir.settings_file()).unwrap();
    sqlitedict::set_item(&dir.metadata_file(), "shakers", &name, &LegacyValue::None).unwrap();
    let opened = Configuration::open_for_runtime(&dir, Platform::Linux, None).unwrap();
    assert_eq!(opened.configuration.metadata_table("shakers").get(&name), Some(&LegacyValue::None));
}

#[test]
fn runtime_migration_does_not_rewrite_an_unrelated_committed_profile() {
    use crate::import::{sqlitedict, value::LegacyValue};
    let (_temporary, dir) = stored(Platform::Linux);
    let mut raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
    raw.get_mut("APP").unwrap().as_dict_mut().unwrap().insert("config_file_version", LegacyValue::from("406"));
    Settings::from_legacy(&raw).0.save(&dir.settings_file()).unwrap();
    let original = sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap();
    sqlitedict::delete_item(&dir.profiles_file(), "unnamed", "forzahorizon6").unwrap();
    let connection = rusqlite::Connection::open(dir.profiles_file()).unwrap();
    connection.execute_batch("CREATE TRIGGER protect_unrelated BEFORE INSERT ON unnamed WHEN NEW.key='emulator' BEGIN SELECT RAISE(ABORT,'unrelated profile must not be rewritten'); END;").unwrap();
    let opened = Configuration::open_for_runtime(&dir, Platform::Linux, None).unwrap();
    assert!(opened.reset.is_none());
    assert_eq!(sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap(), original);
    assert_eq!(
        sqlitedict::read_item(&dir.profiles_file(), "unnamed", "forzahorizon6").unwrap().as_ref(),
        ShippedData::get().master_tunes.get("forzahorizon6")
    );
}

#[test]
fn current_native_start_does_not_read_layout_input_when_v99_is_skipped() {
    for platform in [Platform::Linux, Platform::Windows] {
        let (_temporary, dir) = stored(platform);
        let path = dir.root().join("profiles.json");
        let unused = b"{\"unused\":18446744073709551616}";
        std::fs::write(&path, unused).unwrap();
        let before = [std::fs::read(dir.profiles_file()).unwrap(), std::fs::read(dir.metadata_file()).unwrap()];
        let opened = Configuration::open_for_runtime(&dir, platform, None).unwrap();
        assert!(opened.reset.is_none());
        assert_eq!(std::fs::read(path).unwrap(), unused);
        assert_eq!([std::fs::read(dir.profiles_file()).unwrap(), std::fs::read(dir.metadata_file()).unwrap()], before);
    }
}

#[test]
fn first_native_start_copies_templates_before_a_v99_layout_error() {
    use crate::import::sqlitedict;
    let temporary = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temporary.path());
    let source = dir.root().join("profiles.json");
    std::fs::write(&source, "[]").unwrap();
    let failure = Configuration::open_for_runtime(&dir, Platform::Linux, None).unwrap_err();
    assert!(
        matches!(&failure,StartFailure::Migration(error) if error.version==99 && error.message=="'list' object has no attribute 'items'"),
        "{failure}"
    );
    assert_eq!(std::fs::read(dir.settings_file()).unwrap(), include_bytes!("../../data/srs/template.ini"));
    let data = ShippedData::get();
    assert_eq!(
        sqlitedict::read_item(&dir.profiles_file(), "unnamed", "emulator").unwrap().as_ref(),
        data.template_profiles.get("emulator")
    );
    assert_eq!(
        sqlitedict::read_item(&dir.profiles_file(), "unnamed", "thecrew2").unwrap().as_ref(),
        data.master_tunes.get("thecrew2")
    );
    assert_eq!(sqlitedict::read_table(&dir.metadata_file(), "unnamed").unwrap(), data.template_profiles);
    assert_eq!(
        sqlitedict::table_names(&dir.metadata_file()).unwrap(),
        ["unnamed", "joystick", "shakers", "game_config_notifications"]
    );
    assert_eq!(std::fs::read(source).unwrap(), b"[]");
}

#[test]
fn launcher_flag_reads_and_settings_save_ignore_unrelated_opaque_registry_values() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("config.ini");
    let registry = path.with_file_name("registry.db");
    let mut settings = Settings::default();
    settings.app.start_with_login = false;
    settings.app.show_splash_screen = false;
    settings.save(&path).unwrap();
    let connection = rusqlite::Connection::open(&registry).unwrap();
    connection.execute("INSERT INTO registry (key,value) VALUES ('unrelated',?)", [vec![255_u8]]).unwrap();
    drop(connection);
    let before = std::fs::read(&registry).unwrap();
    let mut loaded = Settings::load(&path).unwrap();
    assert!(!loaded.app.start_with_login, "unrelated decode failure must not discard the selected login flag");
    assert!(!loaded.app.show_splash_screen, "unrelated decode failure must not discard the selected splash flag");
    loaded.app.language = "fr".into();
    loaded.save(&path).unwrap();
    assert!(std::fs::read(&registry).unwrap() == before);
    assert_eq!(Settings::load(&path).unwrap().app.language, "fr");
    // One unreadable selected flag keeps only its own default; the independent
    // valid flag must still be loaded.
    rusqlite::Connection::open(&registry)
        .unwrap()
        .execute("UPDATE registry SET value=? WHERE key='StartWithWindows'", [vec![255_u8]])
        .unwrap();
    let loaded = Settings::load(&path).unwrap();
    assert!(loaded.app.start_with_login);
    assert!(!loaded.app.show_splash_screen);
}

#[test]
fn runtime_start_does_not_reset_for_an_unrelated_registry_decode_error() {
    let temporary = tempfile::tempdir().unwrap();
    let dir = paths::ConfigDir::at(temporary.path());
    let mut first = Configuration::first_run();
    first.settings.app.start_with_login = false;
    first.settings.app.show_splash_screen = false;
    first.save(&dir).unwrap();
    crate::import::sqlitedict::ensure_table(&dir.metadata_file(), "cache").unwrap();
    // Launcher flags now live in registry.db. Keep this deliberately opaque
    // legacy table in metadata.db to exercise the upgrade/read boundary.
    crate::import::sqlitedict::ensure_table(&dir.metadata_file(), "registry").unwrap();
    let connection = rusqlite::Connection::open(dir.metadata_file()).unwrap();
    connection.execute("INSERT INTO registry (key,value) VALUES ('unrelated',?)", [vec![255_u8]]).unwrap();
    drop(connection);
    let before = std::fs::read(dir.metadata_file()).unwrap();
    let opened = Configuration::open_for_runtime(&dir, Platform::Windows, None).unwrap();
    assert!(opened.reset.is_none(), "an unrelated registry row cannot make Windows startup reset the folder");
    assert!(!opened.configuration.settings.app.start_with_login);
    assert!(!opened.configuration.settings.app.show_splash_screen);
    assert!(std::fs::read(dir.metadata_file()).unwrap() == before);
    assert!(!dir.root().join("reset.txt").exists());
}

#[test]
fn desktop_native_assignments_keep_integer_types_unknown_values_and_ini_layout() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.ini");
    std::fs::write(&path,"# geometry\r\n[APP]\r\nwindow_x_pos = 100 # x\r\nwindow_y_pos = 200\r\n[MONITORS]\r\nobsolete = 1\r\n[UNKNOWN]\r\nopaque = retained\r\n").unwrap();
    let mut settings = Settings::load(&path).unwrap();
    settings.app.start_with_login = false;
    settings.app.show_splash_screen = false;
    settings.edit_native(|raw| {
        let app = raw.get_mut("APP").unwrap().as_dict_mut().unwrap();
        // Same projected value still changes the canonical kind from str to int.
        app.insert("window_x_pos", LegacyValue::Int(100));
        let mut monitors = Dict::new();
        monitors.insert("0", LegacyValue::Int(3000));
        raw.insert("MONITORS", monitors.into());
    });
    let raw = settings.native_snapshot();
    assert_eq!(raw.get("APP").unwrap().as_dict().unwrap().get("window_x_pos"), Some(&LegacyValue::Int(100)));
    assert_eq!(raw.get("MONITORS").unwrap().as_dict().unwrap().len(), 1);
    assert!(!settings.app.start_with_login && !settings.app.show_splash_screen);
    settings.save(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# geometry\r\n[APP]\r\nwindow_x_pos = 100 # x\r\n"), "{text:?}");
    assert!(text.contains("opaque = retained\r\n"));
    let loaded = Settings::load(&path).unwrap();
    assert!(!loaded.app.start_with_login && !loaded.app.show_splash_screen);
}

/// The explicit defaults equal what the migration chain produces from the template on a
/// first installation (Windows branch, the default platform): every
/// section, every key.
#[test]
fn defaults_equal_migrated_template() {
    let data = ShippedData::get();
    let mut state = data.fresh_install_state();
    migrate(&mut state, &data.sources(Platform::Windows, &LegacyLayouts::Absent)).unwrap();
    let (migrated, warnings) = Settings::from_legacy(&state.config);
    let defaults = Settings::default();
    assert_eq!(migrated.app, defaults.app);
    assert_eq!(migrated.shakers, defaults.shakers);
    assert_eq!(migrated.wind, defaults.wind);
    assert_eq!(migrated.motion, defaults.motion);
    assert_eq!(migrated.dof_reality, defaults.dof_reality);
    assert_eq!(migrated.prosimu, defaults.prosimu);
    assert_eq!(migrated.yaw_vr, defaults.yaw_vr);
    assert_eq!(migrated.pt_actuator, defaults.pt_actuator);
    assert_eq!(migrated.thanos, defaults.thanos);
    assert_eq!(migrated.vnm, defaults.vnm);
    assert_eq!(migrated.simtech, defaults.simtech);
    assert_eq!(migrated.network_ports, defaults.network_ports);
    assert_eq!(migrated.features, defaults.features);
    assert_eq!(migrated.leds, defaults.leds);
    assert_eq!(migrated.compensation, defaults.compensation);
    assert_eq!(migrated.belt, defaults.belt);
    assert_eq!(migrated.hud, defaults.hud);
    assert_eq!(migrated.iflag, defaults.iflag);
    assert_eq!(migrated.sim3d, defaults.sim3d);
    assert_eq!(migrated.hapticr, defaults.hapticr);
    assert_eq!(migrated.magicbox, defaults.magicbox);
    assert_eq!(migrated.symprojects, defaults.symprojects);
    assert_eq!(migrated, defaults);
    let unexpected: Vec<_> = warnings.iter().filter(|w| !w.is_ignored_key()).collect();
    assert!(unexpected.is_empty(), "{unexpected:?}");
    let not_ported: Vec<&str> = warnings.iter().map(|w| w.location.as_str()).collect();
    assert_eq!(
        not_ported,
        [
            "APP.uuid",
            "APP.enable_cloud_backup",
            "APP.marketing_notifications",
            "APP.show_deactivate_license_button",
            "APP.config_file_version",
            "APP.first_time_installation",
            "INTELLIBOX.config_file_version",
            "APP.window_with"
        ]
    );
}

/// An imported SRS `config.ini` holds sales-gating keys (v101 `trial_subscription_started`, v373
/// `trial_started`, the template's `show_deactivate_license_button`). The import accepts them, reports
/// each as unused and changes no setting. The hidden-page switch (v367 `enable_hapticr`, here
/// switched on) is carried over.
#[test]
fn srs_sales_keys_are_accepted_and_ignored() {
    let data = ShippedData::get();
    let mut state = data.fresh_install_state();
    migrate(&mut state, &data.sources(Platform::Linux, &LegacyLayouts::Absent)).unwrap();
    let (base, _) = Settings::from_legacy(&state.config);
    let app = state.config.get_mut("APP").and_then(LegacyValue::as_dict_mut).unwrap();
    let mut trial = Dict::new();
    trial.insert("premium", LegacyValue::Str("yes".into()));
    app.insert("trial_subscription_started", LegacyValue::Str("yes".into()));
    app.insert("trial_started", trial.into());
    app.insert("enable_hapticr", LegacyValue::Str("yes".into()));
    let (settings, warnings) = Settings::from_legacy(&state.config);
    let mut expected = base.clone();
    expected.app.enable_hapticr = true;
    assert_eq!(settings, expected);
    assert!(!base.app.enable_hapticr, "v367");
    assert!(!warnings.iter().any(|w| w.location == "APP.enable_hapticr"));
    for key in ["trial_subscription_started", "trial_started", "show_deactivate_license_button"] {
        let location = format!("APP.{key}");
        let w = warnings.iter().find(|w| w.location == location).unwrap_or_else(|| panic!("{location} not reported"));
        assert!(w.message.starts_with("not ported"), "{w}");
    }
}

/// A fresh install on the Windows platform keeps the template hardware (there are no installer
/// registry values); the Linux platform (`--original-platform linux`) differs
/// exactly by the magicbox branches v230, v306, v325/v340 and v408.
#[test]
fn first_run_per_platform() {
    use crate::import::{first_run, first_run_for};
    let windows = first_run();
    assert_eq!(windows, first_run_for(Platform::Windows));
    assert_eq!(windows.settings, Settings::default());
    assert_eq!(windows.joysticks, JoystickProfiles::default());
    let s = &windows.settings;
    assert_eq!(s.shakers.sound_card, "None", "tpl");
    assert_eq!(s.shakers.speaker_layout, "1 Speaker - Center", "tpl");
    assert_eq!(s.motion.model, "None", "tpl");
    assert_eq!(s.shortcuts.binding("power_wind_switch").unwrap().buttons, "", "tpl");
    assert_eq!(s.leds.nbr_leds, 150, "tpl");
    let spans: Vec<_> = s.leds.segments.iter().map(|g| (g.start, g.end)).collect();
    assert_eq!(spans, [(1, 50), (51, 100), (101, 125), (126, 150)], "tpl");
    assert_eq!(s.motion.refresh_rate, 30);
    assert!(!s.features.gran_turismo, "v306 on Windows");
    assert!(s.belt.auto_search, "v408 only off Windows");
    assert!(windows.joysticks.profile("Default").unwrap().is_empty(), "v95; v325/v340 off Windows only");

    let linux = first_run_for(Platform::Linux);
    let mut expected = windows.settings.clone();
    expected.shakers.sound_card = "CM106 Like Sound Device Analog Surround 5.1".into();
    expected.shakers.speaker_layout = "SRS UShake6".into();
    expected.motion.model = "H3".into();
    expected.features.gran_turismo = true;
    expected.belt.auto_search = false;
    assert_eq!(linux.settings, expected);
    let leonardo = |kind: &str, trigger: &str| AxisMapping {
        joystick: "Arduino LLC Arduino Leonardo".into(),
        kind: kind.into(),
        trigger: trigger.into(),
        mode: "Single Axis".into(),
        invert_axis: false,
    };
    let pedals = BTreeMap::from([
        ("front".to_owned(), leonardo("axis >", "2")),
        ("back".to_owned(), leonardo("axis <", "4")),
        ("speed".to_owned(), leonardo("axis >", "2")),
    ]);
    assert_eq!(linux.joysticks, JoystickProfiles(BTreeMap::from([("Default".to_owned(), pedals)])));
    assert_eq!(linux.profiles, windows.profiles);
    assert_eq!(linux.shaker_layouts, windows.shaker_layouts);
    assert_eq!(linux.dismissals, windows.dismissals);
}

/// Spot values a fresh install shows.
#[test]
fn fresh_install_values() {
    let s = Settings::default();
    assert_eq!(s.network_ports.get("dirt"), Some(30778), "v213 first installation");
    assert_eq!(s.network_ports.get("ironrebellion"), Some(11631), "template 4123, v404 11631");
    assert_eq!(s.network_ports.get("gta5"), None, "validation rule only");
    assert_eq!(s.dof_reality.smoothing_threshold, 15.0, "v111 150/10 then v374 15");
    assert_eq!(s.dof_reality.boost_threshold, 15.0, "template 150 / 10 at v111");
    assert_eq!(s.prosimu.smoothing_threshold, 0.01, "v105 1 / 100 at v111");
    assert_eq!(s.motion.surge_curving_factor, 1.0, "v134 (validation rule says 1.8)");
    assert_eq!(s.compensation.sensor, CompensationSensor::Virtual, "v354: factory sum is 300");
    assert!(s.compensation.surge.reverse);
    assert!(!s.compensation.yaw.enabled && !s.compensation.heave.enabled && s.compensation.roll.enabled);
    assert!(s.belt.auto_search && !s.hud.auto_search, "v408 off Windows only; HUD v266 no");
    assert!(!s.features.gran_turismo, "v306 on Windows");
    assert_eq!(s.pt_actuator.reaction_speed_map_min, 1000, "v355 2000 then v365 1000");
    assert_eq!(s.wind.fan_count, 2, "v88: emulator wind_curving 1 + 1");
}

/// Reading a version-412 `config.ini`: every scalar is a
/// string, booleans in any accepted spelling, lists comma-separated.
#[test]
fn converts_a_configobj_file() {
    let text = "[APP]\nconfig_file_version = 412\nfirst_time_installation = no\nlog_level = 10\nmeasurement_unit = M\n\
        window_width = 1600\npixel_ratio = 1.25\nx_button_exit = False\nallowed_com_ports = /dev/ttyUSB1, /dev/ttyUSB2\n\
        uuid = abc\n[MOTIONBOX]\nmodel = P3\nrefresh_rate = 30\nboost_multiplier = 2.0\n[[axis_assignment]]\n0 = Pitch\n1 = Roll\n\
        [LED]\nsegment1_type = RF\nsegment1_color = 1, 2, 3\n[NETWORK_PORT]\ndirt = 20777\ngta5 = 20777\n\
        [Yaw VR]\n6dof_mode = no\n[CUSTOM_FEATURES]\n4dvideo_editor_mode = yes\ndams_rear_tl_a = 1.5\n\
        [PORT_FORWARDING]\ndirt = 192.168.1.20:20777,\n[MOTION_COMPENSATION]\ntype = W\n[[surge]]\noffset = 2.5\nenabled = True\nreverse = False\n";
    let config = configobj::parse(text).unwrap();
    let (s, warnings) = Settings::from_legacy(&config);
    assert_eq!(s.app.log_level, 10);
    assert_eq!(s.app.speed_unit, MeasurementUnit::MilesPerHour);
    assert_eq!(s.app.window_width, 1600);
    assert_eq!(s.app.device_pixel_ratio, 1.25);
    assert!(!s.app.close_button_quits);
    assert_eq!(s.app.allowed_serial_ports, ["/dev/ttyUSB1", "/dev/ttyUSB2"]);
    assert_eq!(s.motion.model, "P3");
    assert_eq!(s.motion.motor_axes["0"], "Pitch");
    assert_eq!(s.leds.segments[0].kind, SegmentKind::RpmFlag);
    assert_eq!(s.leds.segments[0].color, [1, 2, 3]);
    assert_eq!(s.leds.segments[1], LedSettings::default().segments[1], "missing keys keep defaults");
    assert_eq!(s.network_ports.get("dirt"), Some(20777));
    assert_eq!(s.network_ports.get("gta5"), Some(20777));
    assert!(!s.yaw_vr.six_dof_mode);
    assert_eq!(s.features.dams_rear_traction_loss_a, Some(1.5));
    assert_eq!(s.port_forwarding["dirt"], ["192.168.1.20:20777"]);
    assert_eq!(s.compensation.sensor, CompensationSensor::Witmotion);
    assert_eq!(s.compensation.surge, CompensationAxis { offset: 2.5, enabled: true, reverse: false });
    assert!(warnings.iter().any(|w| w.location == "APP.uuid"));
    assert!(warnings.iter().any(|w| w.location == "SHAKEBOX" && w.message.contains("section missing")));
}

/// A value that does not convert keeps its default and is reported.
#[test]
fn bad_values_keep_defaults() {
    let config =
        configobj::parse("[APP]\nlog_level = loud\nwindow_width = 900\n[INTELLIBOX]\nwind_type = X\n").unwrap();
    let (s, warnings) = Settings::from_legacy(&config);
    assert_eq!(s.app.log_level, 30);
    assert_eq!(s.app.window_width, 900);
    assert_eq!(s.wind.fan_kind, WindType::Fan);
    assert!(warnings.iter().any(|w| w.location == "APP.log_level"));
    assert!(warnings.iter().any(|w| w.location == "INTELLIBOX.wind_type"));
}

#[test]
fn toml_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let mut s = Settings::default();
    s.motion.model = "P3".into();
    s.network_ports.set("dirt", 20777);
    s.port_forwarding.insert("dirt".into(), vec!["10.0.0.2:20777".into()]);
    s.save(&path).unwrap();
    assert_eq!(Settings::load(&path).unwrap(), s);
    assert_eq!(Settings::load(&dir.path().join("missing.toml")).unwrap(), Settings::default());
}

#[test]
fn native_ini_keeps_raw_values_and_unknown_keys_when_another_setting_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.ini");
    std::fs::write(&path, "[APP]\nconfig_file_version = 412\nfirst_time_installation = no\nlog_level = invalid\nstartup_page = Wind\nunknown = 'a,b',\n[SHORTCUTS]\nz_keys = ctrl+z\na_keys = ctrl+a\n[LED]\nsegment1_color = 1, 2, 3\n").unwrap();
    let mut settings = Settings::load(&path).unwrap();
    settings.app.start_page = "Motion".into();
    settings.save(&path).unwrap();
    let raw = crate::import::configobj::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let app = raw.get("APP").unwrap().as_dict().unwrap();
    assert_eq!(app.get("log_level"), Some(&"invalid".into()));
    assert_eq!(app.get("startup_page"), Some(&"Motion".into()));
    assert_eq!(app.get("unknown"), Some(&LegacyValue::List(vec!["a,b".into()])));
    assert_eq!(raw.get("SHORTCUTS").unwrap().as_dict().unwrap().str_keys(), ["z_keys", "a_keys"]);
    assert_eq!(
        app.str_keys(),
        ["config_file_version", "first_time_installation", "log_level", "startup_page", "unknown"]
    );
}

#[test]
fn native_ini_edits_use_original_enum_and_flat_led_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.ini");
    let mut settings = Settings::default();
    settings.app.speed_unit = MeasurementUnit::MilesPerHour;
    settings.app.start_with_login = false;
    settings.app.show_splash_screen = false;
    settings.compensation.sensor = CompensationSensor::Witmotion;
    settings.leds.segments[0].kind = SegmentKind::SpeedFlag;
    settings.shortcuts.bindings.get_mut("power_wind_switch").unwrap().keys = "ctrl+k".into();
    settings.save(&path).unwrap();
    let raw = crate::import::configobj::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let section = |name| raw.get(name).unwrap().as_dict().unwrap();
    assert_eq!(section("APP").get("measurement_unit"), Some(&"M".into()));
    assert_eq!(section("MOTION_COMPENSATION").get("type"), Some(&"W".into()));
    assert_eq!(section("LED").get("segment1_type"), Some(&"SF".into()));
    assert_eq!(section("SHORTCUTS").get("power_wind_switch_keys"), Some(&"ctrl+k".into()));
    assert!(!section("APP").contains("start_with_login"));
    assert_eq!(Settings::load(&path).unwrap(), settings);
}

#[test]
fn native_files_are_the_only_new_store_files_and_existing_rust_stores_upgrade() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = super::paths::ConfigDir::at(tmp.path());
    let old = Configuration::first_run();
    old.settings.save(&dir.old_settings_file()).unwrap();
    std::fs::create_dir(dir.tunes_dir()).unwrap();
    for (game, tune) in old.profiles.games() {
        std::fs::write(dir.tunes_dir().join(format!("{game}.toml")), toml::to_string(tune).unwrap()).unwrap();
    }
    super::paths::write_toml(&dir.shaker_layouts_file(), &old.shaker_layouts).unwrap();
    super::paths::write_toml(&dir.joysticks_file(), &old.joysticks).unwrap();
    super::paths::write_toml(&dir.state_file(), &old.dismissals).unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert_eq!(opened.configuration, old);
    assert!(dir.settings_file().is_file() && dir.profiles_file().is_file() && dir.metadata_file().is_file());
    assert!(dir.old_settings_file().is_file() && dir.tunes_dir().is_dir());
    let fresh = tempfile::tempdir().unwrap();
    let fresh_dir = super::paths::ConfigDir::at(fresh.path());
    Configuration::open_for(&fresh_dir, Platform::Windows).unwrap();
    let mut files: Vec<_> = std::fs::read_dir(fresh.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(files, ["config.ini", "metadata.db", "profile.db", "registry.db"]);
}

#[test]
fn stored_old_native_ini_runs_the_remaining_migrations_in_the_product() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = super::paths::ConfigDir::at(tmp.path());
    let data = ShippedData::get();
    std::fs::write(dir.settings_file(), crate::import::configobj::encode(&data.template_config).unwrap()).unwrap();
    crate::import::sqlitedict::replace_table(&dir.profiles_file(), "unnamed", &data.template_profiles).unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(opened.first_installation);
    assert_eq!(opened.configuration, Configuration::first_run());
    assert_eq!(Settings::load(&dir.settings_file()).unwrap(), opened.configuration.settings);
}

#[test]
fn production_start_copies_legacy_database_only_for_missing_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = super::paths::ConfigDir::at(tmp.path());
    let original = Configuration::first_run();
    original.save(&dir).unwrap();
    let legacy = dir.root().join("config.db");
    std::fs::rename(dir.profiles_file(), &legacy).unwrap();
    crate::import::sqlitedict::set_item(&legacy, "unrelated", "kept", &LegacyValue::Bytes(vec![3, 2, 1])).unwrap();
    let before_metadata = std::fs::read(dir.metadata_file()).unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert_eq!(opened.configuration, original);
    assert_eq!(
        crate::import::sqlitedict::read_table(&dir.profiles_file(), "unrelated").unwrap().get("kept"),
        Some(&LegacyValue::Bytes(vec![3, 2, 1]))
    );
    assert_eq!(
        std::fs::read(dir.metadata_file()).unwrap(),
        before_metadata,
        "existing metadata is not copied over or rewritten"
    );
}

/// A different value of the same kind: booleans flip (also as yes/no text), numbers grow by
/// one, other text gets a suffix, a list's last item changes (an empty list gets one).
fn perturbed(value: &LegacyValue) -> LegacyValue {
    match value {
        LegacyValue::Bool(b) => LegacyValue::Bool(!b),
        LegacyValue::Int(i) => LegacyValue::Int(i + 1),
        LegacyValue::Float(f) => LegacyValue::Float(f + 1.0),
        LegacyValue::Str(s) => LegacyValue::Str(match (str2bool(s), parse_py_int(s), parse_py_float(s)) {
            (Some(b), _, _) => if b { "no" } else { "yes" }.to_owned(),
            (None, Some(i), _) => (i + 1).to_string(),
            (None, None, Some(f)) => (f + 1.0).to_string(),
            (None, None, None) => format!("{s}x"),
        }),
        LegacyValue::List(items) => {
            let mut items = items.clone();
            match items.last_mut() {
                Some(last) => *last = perturbed(last),
                None => items.push(LegacyValue::Str("x".into())),
            }
            LegacyValue::List(items)
        }
        _ => LegacyValue::Str("x".into()),
    }
}

/// Paths of the scalar and list entries of a section; subsections are descended into.
fn leaf_paths(dict: &Dict, prefix: &[String], out: &mut Vec<Vec<String>>) {
    for (key, value) in dict.iter() {
        let mut path = prefix.to_vec();
        path.push(key.to_text());
        match value {
            LegacyValue::Dict(sub) => leaf_paths(sub, &path, out),
            _ => out.push(path),
        }
    }
}

fn leaf_mut<'a>(config: &'a mut Dict, section: &str, path: &[String]) -> &'a mut LegacyValue {
    let mut value = config.get_mut(section).unwrap();
    for key in path {
        value = value.as_dict_mut().unwrap().get_mut(key).unwrap();
    }
    value
}

/// Every key the migration chain leaves in config.ini reaches a settings field under openrigcue's own
/// names: changing its value changes the converted settings, or the value is reported for that
/// key. Only the keys listed as unused are exempt.
#[test]
fn every_migrated_config_key_is_carried_over() {
    every_migrated_config_key_is_carried_over_on(Platform::Windows);
    every_migrated_config_key_is_carried_over_on(Platform::Linux);
}

#[test]
fn ui_settings_view_and_assignments_keep_native_values_instead_of_typed_defaults() {
    use serde_json::json;
    let settings = Configuration::first_run().settings;
    let mut raw = settings.native_snapshot();
    let motion = raw.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap();
    motion.remove("reaction_speed");
    motion.insert("refresh_rate", "fast".into());
    motion.insert("sway_clip_factor", LegacyValue::List(vec!["one".into(), "two".into()]));
    let settings = Settings::from_legacy(&raw).0;
    let view = settings.ui_view().unwrap();
    assert!(view["motion"].get("reaction_speed").is_none());
    assert_eq!(view["motion"]["refresh_rate"], "fast");
    assert_eq!(view["motion"]["sway_clip_factor"], json!(["one", "two"]));
    let edited = settings.apply_ui_patch(&json!({"motion":{"model":"P3"},"app":{"start_with_login":true}})).unwrap();
    let snapshot = edited.native_snapshot();
    let mut expected = raw;
    expected.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap().insert("model", "P3".into());
    assert_eq!(snapshot, expected, "only the assigned native key changed");
    assert!(edited.app.start_with_login);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.ini");
    edited.save(&path).unwrap();
    let restored = Settings::load(&path).unwrap();
    let stored_values = crate::import::configobj::parse(&crate::import::configobj::encode(&expected).unwrap()).unwrap();
    assert_eq!(restored.native_snapshot(), stored_values);
    assert!(restored.app.start_with_login);
}

#[test]
fn complete_native_settings_ui_view_roundtrips_on_both_reference_platforms() {
    for platform in [Platform::Windows, Platform::Linux] {
        let settings = &crate::import::first_run_for(platform).settings;
        let view = settings.ui_view().unwrap();
        let restored = settings.apply_ui_patch(&view).unwrap();
        assert!(restored == *settings, "UI assignments changed the effective settings on {platform:?}");
        assert_eq!(restored.app.start_with_login, settings.app.start_with_login);
    }
}

#[test]
fn settings_axis_assignment_keeps_its_other_native_fields() {
    let settings = Configuration::first_run().settings;
    let before = settings.native_snapshot();
    let edited = settings.apply_ui_patch(&serde_json::json!({"compensation":{"yaw":{"enabled":true}}})).unwrap();
    let mut expected = before;
    expected
        .get_mut("MOTION_COMPENSATION")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .get_mut("yaw")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .insert("enabled", "yes".into());
    assert_eq!(edited.native_snapshot(), expected);
}

fn every_migrated_config_key_is_carried_over_on(platform: Platform) {
    let data = ShippedData::get();
    let mut state = data.fresh_install_state();
    migrate(&mut state, &data.sources(platform, &LegacyLayouts::Absent)).unwrap();
    let (base, _) = Settings::from_legacy(&state.config);
    let mut dropped = Vec::new();
    let mut probed = 0;
    for (section_key, section) in state.config.iter() {
        let section_name = section_key.to_text();
        let mut paths = Vec::new();
        leaf_paths(section.as_dict().unwrap(), &[], &mut paths);
        for path in paths {
            if NOT_PORTED.contains(&(section_name.as_str(), path[0].as_str())) {
                continue;
            }
            let mut config = state.config.clone();
            let leaf = leaf_mut(&mut config, &section_name, &path);
            *leaf = perturbed(leaf);
            let (mut settings, warnings) = Settings::from_legacy(&config);
            let location = format!("{section_name}.{}", path[0]);
            let reported = warnings.iter().any(|w| w.location == location);
            if settings == base && !reported {
                dropped.push(format!("{section_name}.{}", path.join(".")));
            }
            settings.retain_source(&base);
            let reloaded = Settings::from_legacy(&settings.native_snapshot()).0;
            assert_eq!(reloaded, settings, "native write-back for {section_name}/{}", path.join("/"));
            probed += 1;
        }
    }
    assert!(probed > 600, "{probed} keys probed");
    assert!(dropped.is_empty(), "keys without a settings field: {dropped:?}");
}

/// Start with migration: the first start writes the first-run configuration of
/// the platform and reports the first installation; the next start reads it back and does not.
#[test]
fn first_start_writes_the_configuration() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = super::paths::ConfigDir::at(tmp.path().join("config"));
    let first = Configuration::open_for(&dir, Platform::Linux).unwrap();
    assert!(first.first_installation);
    assert_eq!(&first.configuration, crate::import::first_run_for(Platform::Linux));
    assert!(dir.settings_file().is_file() && dir.metadata_file().is_file());
    let second = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(!second.first_installation);
    assert_eq!(second.configuration, first.configuration, "the stored files, not the Windows first run");
}

fn stored(platform: Platform) -> (tempfile::TempDir, super::paths::ConfigDir) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = super::paths::ConfigDir::at(tmp.path().join("config"));
    crate::import::first_run_for(platform).save(&dir).unwrap();
    // A completed startup has opened its cache dictionary too.
    crate::import::sqlitedict::ensure_table(&dir.metadata_file(), "cache").unwrap();
    (tmp, dir)
}

/// A missing `config.ini` with `profile.db` and `metadata.db`
/// present: the template `config.ini` (version 84, first installation) runs every step over the
/// stored databases. v126 sets `gear_smoothing_factor` 0 on every tune, v215 drops `ccd`, v99 adds
/// the shipped layouts next to the user's, v123 empties the notification tables.
#[test]
fn start_without_settings_runs_the_template_chain_over_the_stored_databases() {
    let (_tmp, dir) = stored(Platform::Windows);
    let mut configuration = crate::import::first_run().clone();
    let mut ac = configuration.profiles.get("ac").unwrap().clone();
    ac.gear_smoothing_factor = 5.0;
    configuration.profiles.insert("ccd", ac.clone());
    configuration.profiles.insert("ac", ac);
    let mine = configuration.shaker_layouts.0["1 Speaker - Center"].clone();
    configuration.shaker_layouts.0.insert("Mine".into(), mine);
    configuration.dismissals.game_setup.insert("ac".into());
    configuration.save(&dir).unwrap();
    std::fs::remove_file(dir.settings_file()).unwrap();

    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(opened.first_installation, "template first_time_installation = yes");
    assert_eq!(opened.reset, None);
    let c = &opened.configuration;
    let ac = c.profiles.get("ac").unwrap();
    assert_eq!(ac.gear_smoothing_factor, 95.0, "v126 sets 0, a later step every tune 95; the user's 5 is gone");
    assert_eq!(ac, crate::import::first_run().profiles.get("ac").unwrap());
    assert!(c.profiles.get("ccd").is_none(), "v215");
    assert!(!crate::import::sqlitedict::read_table(&dir.profiles_file(), "unnamed").unwrap().contains("ccd"));
    assert!(c.shaker_layouts.0.contains_key("Mine"), "v99 inserts the shipped layouts, keeps the others");
    assert_eq!(c.dismissals, Dismissals::default(), "v123");
    assert_eq!(c.settings, crate::import::first_run().settings, "settings come from the template");
    assert_eq!(&Configuration::load(&dir).unwrap(), c, "written");
}

/// A missing `profile.db` with `config.ini` present: no step runs on a
/// version-412 `config.ini`, so `profile.db` is the template (70 games).
#[test]
fn start_without_tunes_copies_the_template_profiles() {
    let (_tmp, dir) = stored(Platform::Windows);
    std::fs::remove_file(dir.profiles_file()).unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(!opened.first_installation);
    assert_eq!(opened.reset, None);
    let (template, _) = crate::import::template_profile_store();
    assert_eq!(opened.configuration.profiles, template);
    // The template tunes have the version-84 shape: none converts, so each is the game's factory
    // tune and `ccd`, which has none, is left out.
    assert_eq!(ShippedData::get().template_profiles.len(), 70);
    assert_eq!(template.games().count(), 69);
    assert!(template.get("ccd").is_none());
    assert_eq!(ProfileStore::load(&dir.profiles_file()).unwrap(), template, "written");
    assert_eq!(
        crate::import::sqlitedict::read_table(&dir.profiles_file(), "unnamed").unwrap(),
        ShippedData::get().template_profiles
    );
    assert_eq!(opened.configuration.settings, crate::import::first_run().settings);
}

/// The template `metadata.db` has no `joystick` or notification table: a missing file is an empty
/// table, written at once.
#[test]
fn start_without_joysticks_or_state_has_empty_tables() {
    let (_tmp, dir) = stored(Platform::Linux);
    rusqlite::Connection::open(dir.metadata_file())
        .unwrap()
        .execute_batch("DROP TABLE joystick; DROP TABLE notifications; DROP TABLE game_config_notifications;")
        .unwrap();
    let opened = Configuration::open_for(&dir, Platform::Linux).unwrap();
    assert_eq!(opened.configuration.joysticks, JoystickProfiles(BTreeMap::new()));
    assert_eq!(opened.configuration.dismissals, Dismissals::default());
    let names = crate::import::sqlitedict::table_names(&dir.metadata_file()).unwrap();
    assert!(names.contains(&"joystick".into()) && names.contains(&"notifications".into()));
}

/// The start fails when the selected layout is not in the shakers table (here the
/// template `metadata.db` replaced a missing one) or `profile.db` has no `emulator`, with the corrupted-file
/// text. The Windows platform then writes `reset.txt`, deletes the files and starts again from
/// the templates; the Linux platform stops.
#[test]
fn failed_start_resets_on_windows_and_stops_on_linux() {
    for platform in [Platform::Windows, Platform::Linux] {
        let (_tmp, dir) = stored(platform);
        std::fs::remove_file(dir.metadata_file()).unwrap();
        let result = Configuration::open_for(&dir, platform);
        match platform {
            Platform::Windows => {
                let opened = result.unwrap();
                assert!(opened.first_installation);
                let expected = format!(
                    "{} file is corrupted. Please delete it and restart OpenRigCue. More info: check {} \
                     or open OpenRigCue Help from the application header. Error message: \
                     '1 Speaker - Center'",
                    dir.settings_file().display(),
                    dir.root().join("log.txt").display()
                );
                assert_eq!(opened.reset.as_deref(), Some(expected.as_str()));
                assert!(!dir.root().join(RESET_FILE).exists(), "reset_config_files deletes it");
                assert_eq!(&opened.configuration, crate::import::first_run_for(platform));
                assert_eq!(&Configuration::load_for(&dir, platform).unwrap(), crate::import::first_run_for(platform));
            }
            Platform::Linux => {
                let error = result.unwrap_err();
                assert!(
                    matches!(&error, StartFailure::MissingSpeakerLayout { layout, .. } if layout == "SRS UShake6"),
                    "{error}"
                );
                assert!(error.to_string().ends_with("Error message: 'SRS UShake6'"), "{error}");
                assert_eq!(read_shaker_layouts(&dir), ShakerLayouts::default(), "the template table was written");
                assert!(!dir.root().join(RESET_FILE).exists());
            }
        }
    }

    let (_tmp, dir) = stored(Platform::Windows);
    crate::import::sqlitedict::delete_item(&dir.profiles_file(), "unnamed", "emulator").unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(opened.reset.unwrap().ends_with("Error message: 'emulator'"));
    assert!(crate::import::sqlitedict::read_table(&dir.profiles_file(), "unnamed").unwrap().contains("emulator"));
    assert_eq!(&opened.configuration, crate::import::first_run());

    let (_tmp, dir) = stored(Platform::Linux);
    crate::import::sqlitedict::delete_item(&dir.profiles_file(), "unnamed", "emulator").unwrap();
    assert!(matches!(Configuration::open_for(&dir, Platform::Linux), Err(StartFailure::MissingEmulatorTune { .. })));
}

#[test]
fn missing_layout_message_retains_native_unicode_key_repr() {
    let (_tmp, dir) = stored(Platform::Linux);
    let layout = "Unknown\u{00a0}Layout\u{200b}\n'quoted'";
    let mut raw = Settings::load(&dir.settings_file()).unwrap().native_snapshot();
    raw.get_mut("SHAKEBOX").unwrap().as_dict_mut().unwrap().insert("profile", layout.into());
    Settings::from_legacy(&raw).0.write_ini_only(&dir.settings_file()).unwrap();
    let error = Configuration::open_for(&dir, Platform::Linux).unwrap_err();
    assert!(matches!(&error, StartFailure::MissingSpeakerLayout { layout: actual, .. } if actual == layout));
    assert!(error.to_string().ends_with("Error message: \"Unknown\\xa0Layout\\u200b\\n'quoted'\""), "{error}");
}

/// With `reset.txt` present (a previous start failed), the
/// start deletes `config.db`, `config.ini`, `log.txt`, `reset.txt`, `restore.txt`, `metadata.db`
/// and `profile.db` first, so it runs from the templates; other files stay.
#[test]
fn leftover_reset_file_resets_the_next_start() {
    let (_tmp, dir) = stored(Platform::Windows);
    let mut changed = crate::import::first_run().clone();
    changed.settings.app.start_page = "Motion".into();
    changed.save(&dir).unwrap();
    for name in [RESET_FILE, "config.db", "log.txt", "restore.txt", "keep.txt"] {
        std::fs::write(dir.root().join(name), "x").unwrap();
    }
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(opened.first_installation);
    assert_eq!(opened.reset, None, "nothing raised in this run");
    assert_eq!(&opened.configuration, crate::import::first_run());
    for name in [RESET_FILE, "config.db", "log.txt", "restore.txt"] {
        assert!(!dir.root().join(name).exists(), "{name}");
    }
    assert!(dir.root().join("keep.txt").is_file());

    assert!(!reset_config_files(&dir), "no reset.txt: nothing is deleted");
    assert!(dir.settings_file().is_file());
}

/// `config.ini` is written on every start, also when no step runs.
#[test]
fn every_start_writes_the_settings() {
    let (_tmp, dir) = stored(Platform::Windows);
    std::fs::write(
        dir.settings_file(),
        "# stored comment\n[APP]\nconfig_file_version=412\nfirst_time_installation=no\n",
    )
    .unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(!opened.first_installation);
    let written = std::fs::read_to_string(dir.settings_file()).unwrap();
    assert_eq!(written, "# stored comment\n[APP]\nconfig_file_version = 412\nfirst_time_installation = no\n");
}

/// The first start's chain reads `<docs>/profiles.json` at v99 and deletes it; v211 deletes
/// `<docs>/crash.txt`. A file that is valid JSON but not an object fails v99 before the delete.
#[test]
fn first_start_consumes_profiles_json_and_crash_txt() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = super::paths::ConfigDir::at(tmp.path().join("config"));
    std::fs::create_dir_all(dir.root()).unwrap();
    std::fs::write(dir.root().join("profiles.json"), "[]").unwrap();
    let error = Configuration::open_for(&dir, Platform::Linux).unwrap_err();
    assert!(matches!(&error, StartFailure::Migration(e) if e.version == 99), "{error}");
    assert!(dir.root().join("profiles.json").is_file());

    let shipped: serde_json::Value = serde_json::from_str(include_str!("../../data/srs/shaker_layouts.json")).unwrap();
    let json = serde_json::json!({ "1 Speaker - Center": shipped["1 Speaker - Center"] });
    std::fs::write(dir.root().join("profiles.json"), json.to_string()).unwrap();
    std::fs::write(dir.root().join("crash.txt"), "old").unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert_eq!(&opened.configuration, crate::import::first_run());
    assert!(!dir.root().join("profiles.json").exists());
    assert!(!dir.root().join("crash.txt").exists());
}

/// A stored file that does not read makes the start fail as well (INI file / database).
#[test]
fn unreadable_settings_reset_on_windows() {
    let (_tmp, dir) = stored(Platform::Windows);
    std::fs::write(dir.settings_file(), "[app\n").unwrap();
    let opened = Configuration::open_for(&dir, Platform::Windows).unwrap();
    assert!(opened.reset.is_some() && opened.first_installation);
    assert_eq!(&opened.configuration, crate::import::first_run());
    let (_tmp, dir) = stored(Platform::Linux);
    std::fs::write(dir.settings_file(), "[app\n").unwrap();
    assert!(matches!(Configuration::open_for(&dir, Platform::Linux), Err(StartFailure::Store(_))));
}

fn read_shaker_layouts(dir: &super::paths::ConfigDir) -> ShakerLayouts {
    Configuration::load(dir).unwrap().shaker_layouts
}

#[test]
fn ini_ui_edit_preserves_the_shipped_configobj_layout_reference() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("../../data/configobj-reference.json")).unwrap();
    let case = fixture["cases"].as_array().unwrap().iter().find(|case| case["name"] == "comments-indent").unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.ini");
    std::fs::write(&path, case["source"].as_str().unwrap()).unwrap();
    let settings = Settings::load(&path).unwrap();
    let edited = settings.apply_ui_patch(&serde_json::json!({"app":{"x":"3","new":"yes"}})).unwrap();
    edited.save(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), case["output"].as_str().unwrap());
}

#[test]
fn native_start_keeps_ini_comments_bom_and_crlf() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = super::paths::ConfigDir::at(tmp.path());
    Configuration::first_run().save(&dir).unwrap();
    let body = std::fs::read_to_string(dir.settings_file()).unwrap();
    let body = body.replace("log_level = 30", "log_level = 30    # keep inline");
    let original = format!("\u{feff}# startup comment\n\n{body}").replace('\n', "\r\n");
    std::fs::write(dir.settings_file(), original.as_bytes()).unwrap();
    Configuration::open_for(&dir, crate::import::migrate::Platform::Linux).unwrap();
    assert_eq!(std::fs::read(dir.settings_file()).unwrap(), original.as_bytes());
}

#[test]
fn windows_runtime_start_preserves_unrelated_malformed_profile_instead_of_resetting() {
    use crate::import::sqlitedict;
    let (_temporary, dir) = stored(Platform::Windows);
    let path = dir.profiles_file();
    sqlitedict::tests::write_table(&path, "unnamed", &[("opaque", &[255])]);
    let before = std::fs::read(&path).unwrap();
    let opened = Configuration::open_for_runtime(&dir, Platform::Windows, None).unwrap();
    assert!(opened.reset.is_none());
    assert!(!opened.first_installation);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(sqlitedict::read_item(&path, "unnamed", "opaque").is_err());
    assert!(!dir.root().join("reset.txt").exists());
}

/// An assigned dictionary can be an object its assigner still holds. Reading the settings with a typed edit
/// outstanding shows the edit and leaves that object as it was, so the typed field set back to its earlier value
/// reads as that value again.
#[test]
fn reading_the_native_snapshot_writes_no_typed_edit_into_a_shared_dictionary() {
    use openrigcue_model::native_value::{LegacyValue, NativeObject};
    let read = |settings: &Settings| {
        let yaw = settings.native_snapshot().get("MOTION_COMPENSATION").unwrap().item_owned("yaw").unwrap();
        yaw.item_owned("enabled").unwrap()
    };
    let mut settings = Configuration::first_run().settings;
    let yaw = settings.native_snapshot().get("MOTION_COMPENSATION").unwrap().item_owned("yaw").unwrap();
    let shared = LegacyValue::Object(NativeObject::new(yaw));
    settings.edit_native(|raw| {
        raw.get_mut("MOTION_COMPENSATION").unwrap().assign_item("yaw", shared.clone()).unwrap();
    });
    let stored = shared.item_owned("enabled").unwrap();
    assert_eq!(read(&settings), stored);
    let enabled = settings.compensation.yaw.enabled;
    settings.compensation.yaw.enabled = !enabled;
    assert_ne!(read(&settings), stored, "the typed edit is read");
    assert_eq!(shared.item_owned("enabled").unwrap(), stored, "reading wrote into the shared dictionary");
    settings.compensation.yaw.enabled = enabled;
    assert_eq!(read(&settings), stored, "the typed field is back at its stored value");
}

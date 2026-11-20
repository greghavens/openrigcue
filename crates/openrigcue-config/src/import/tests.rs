use std::path::Path;

use super::*;
use crate::import::sqlitedict::tests::write_table;
use crate::import::value::Key;
use crate::settings::Settings;

/// Protocol-2 pickle of a legacy value (what sqlitedict stores), for building fixture databases.
fn pickle(root: &LegacyValue) -> Vec<u8> {
    fn key(k: &Key, out: &mut Vec<u8>) {
        match k {
            Key::Str(s) => value(&LegacyValue::Str(s.clone()), out),
            Key::Int(i) => value(&LegacyValue::Int(*i), out),
            Key::Bool(v) => value(&LegacyValue::Bool(*v), out),
            Key::Float(v) => value(&LegacyValue::Float(v.value()), out),
            Key::None => value(&LegacyValue::None, out),
            Key::Extra(k) => value(&k.to_value(), out),
        }
    }
    fn value(v: &LegacyValue, out: &mut Vec<u8>) {
        match v {
            LegacyValue::None => out.push(b'N'),
            LegacyValue::Bool(true) => out.push(0x88),
            LegacyValue::Bool(false) => out.push(0x89),
            LegacyValue::Int(i) => {
                out.push(0x8a);
                out.push(8);
                out.extend_from_slice(&i.to_le_bytes());
            }
            LegacyValue::Float(f) => {
                out.push(b'G');
                out.extend_from_slice(&f.to_be_bytes());
            }
            LegacyValue::Str(s) => {
                out.push(b'X');
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            LegacyValue::List(items) => {
                out.extend_from_slice(b"](");
                items.iter().for_each(|i| value(i, out));
                out.push(b'e');
            }
            LegacyValue::Dict(d) => {
                out.extend_from_slice(b"}(");
                for (k, v) in d.iter() {
                    key(k, out);
                    value(v, out);
                }
                out.push(b'u');
            }
            other => panic!("fixture encoder does not need {}", other.type_name()),
        }
    }
    let mut out = vec![0x80, 2];
    value(root, &mut out);
    out.push(b'.');
    out
}

/// INI text: scalars as plain text, lists comma-joined (`x,` for one item), dicts as subsections.
fn configobj_text(config: &Dict) -> String {
    fn scalar(v: &LegacyValue) -> String {
        match v {
            LegacyValue::Str(s) if s.is_empty() => "\"\"".into(),
            LegacyValue::Str(s) if s.contains(',') => format!("\"{s}\""),
            LegacyValue::List(items) if items.is_empty() => ",".into(),
            LegacyValue::List(items) if items.len() == 1 => format!("{},", scalar(&items[0])),
            LegacyValue::List(items) => items.iter().map(scalar).collect::<Vec<_>>().join(", "),
            other => other.py_str().unwrap(),
        }
    }
    fn section(name: &str, d: &Dict, depth: usize, out: &mut String) {
        out.push_str(&format!("{}{}{}\n", "[".repeat(depth), name, "]".repeat(depth)));
        for (k, v) in d.iter() {
            if !matches!(v, LegacyValue::Dict(_)) {
                let key = k.to_text();
                let key = if key.trim() != key { format!("\"{key}\"") } else { key };
                out.push_str(&format!("{key} = {}\n", scalar(v)));
            }
        }
        for (k, v) in d.iter() {
            if let LegacyValue::Dict(sub) = v {
                section(&k.to_text(), sub, depth + 1, out);
            }
        }
    }
    let mut out = String::new();
    for (name, v) in config.iter() {
        section(&name.to_text(), v.as_dict().unwrap(), 1, &mut out);
    }
    out
}

fn write_folder(dir: &Path, state: &migrate::LegacyState) {
    write_folder_with(dir, state, "profile.db", "metadata.db");
}

/// `config.ini` plus the profile table in `profile_db` and the metadata tables in `metadata_db`.
fn write_folder_with(dir: &Path, state: &migrate::LegacyState, profile_db: &str, metadata_db: &str) {
    std::fs::write(dir.join("config.ini"), configobj_text(&state.config)).unwrap();
    let rows = |d: &Dict| d.iter().map(|(k, v)| (k.to_text(), pickle(v))).collect::<Vec<_>>();
    let profile_rows = rows(&state.profiles);
    let refs: Vec<(&str, &[u8])> = profile_rows.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
    write_table(&dir.join(profile_db), "unnamed", &refs);
    for (table, d) in [("shakers", &state.shaker_layouts), ("joystick", &state.joysticks)] {
        let r = rows(d);
        let refs: Vec<(&str, &[u8])> = r.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
        write_table(&dir.join(metadata_db), table, &refs);
    }
    let dismissed = pickle(&LegacyValue::Bool(true));
    write_table(&dir.join(metadata_db), "game_config_notifications", &[("ac", &dismissed)]);
    write_table(&dir.join(metadata_db), "notifications", &[("backend.controllers.smc3", &dismissed)]);
}

fn fresh_state() -> migrate::LegacyState {
    let data = shipped::ShippedData::get();
    let mut state = data.fresh_install_state();
    migrate::migrate(&mut state, &data.sources(Platform::Windows, &LegacyLayouts::Absent)).unwrap();
    state
}

/// A current (v412) SRS folder imports without running steps and converts every store.
#[test]
fn imports_a_current_srs_folder() {
    let dir = tempfile::tempdir().unwrap();
    write_folder(dir.path(), &fresh_state());
    let imported = import_srs_folder(dir.path(), Platform::Windows).unwrap();
    assert!(imported.report.applied.is_empty());
    let unexpected: Vec<_> = imported.warnings.iter().filter(|w| !w.is_ignored_key()).collect();
    assert!(unexpected.is_empty(), "{unexpected:?}");
    let first = first_run();
    assert_eq!(imported.configuration.settings, first.settings, "same values through config.ini text");
    assert_eq!(imported.configuration.profiles, first.profiles, "same values through pickled profile.db");
    assert_eq!(imported.configuration.shaker_layouts, first.shaker_layouts);
    assert_eq!(imported.configuration.joysticks, first.joysticks);
    assert!(imported.configuration.dismissals.game_setup_dismissed("ac"));
    assert!(imported.configuration.dismissals.driver_dismissed("backend.controllers.smc3"));
}

/// A legacy folder with only `config.db` reads the profile table and the
/// metadata tables from it.
#[test]
fn imports_a_legacy_config_db_folder() {
    let dir = tempfile::tempdir().unwrap();
    write_folder_with(dir.path(), &fresh_state(), "config.db", "config.db");
    let imported = import_srs_folder(dir.path(), Platform::Windows).unwrap();
    let first = first_run();
    assert_eq!(imported.configuration.profiles, first.profiles);
    assert_eq!(imported.configuration.shaker_layouts, first.shaker_layouts);
    assert_eq!(imported.configuration.joysticks, first.joysticks);
    assert!(imported.configuration.dismissals.game_setup_dismissed("ac"));
    assert!(imported.configuration.dismissals.driver_dismissed("backend.controllers.smc3"));
}

/// `config.db` stands in only for a missing file: an existing `profile.db` wins
/// and only the missing `metadata.db` comes from `config.db`.
#[test]
fn legacy_config_db_fills_only_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state();
    write_folder_with(dir.path(), &state, "profile.db", "config.db");
    let legacy_profile = pickle(&LegacyValue::Dict(Dict::new()));
    write_table(&dir.path().join("config.db"), "unnamed", &[("ac", &legacy_profile)]);
    let imported = import_srs_folder(dir.path(), Platform::Windows).unwrap();
    assert_eq!(imported.configuration.profiles, first_run().profiles, "profile.db read, not config.db");
    assert_eq!(imported.configuration.shaker_layouts, first_run().shaker_layouts, "metadata from config.db");
}

/// The template as an existing (not first-time) installation at version 83.
fn old_state() -> migrate::LegacyState {
    let mut state = shipped::ShippedData::get().fresh_install_state();
    let app = state.config.get_mut("APP").unwrap().as_dict_mut().unwrap();
    app.insert("first_time_installation", LegacyValue::Str("no".into()));
    state
}

/// Step v123: a folder older than 123 loses every metadata.db table except `joystick`
/// and `shakers`, so its filled notification tables import as no dismissals (also from a legacy
/// `config.db`).
#[test]
fn import_before_v123_drops_dismissals() {
    for (profile_db, metadata_db) in [("profile.db", "metadata.db"), ("config.db", "config.db")] {
        let dir = tempfile::tempdir().unwrap();
        write_folder_with(dir.path(), &old_state(), profile_db, metadata_db);
        let imported = import_srs_folder(dir.path(), Platform::Windows).unwrap();
        assert!(imported.report.from_version < 123.into());
        assert_eq!(imported.configuration.dismissals, Dismissals::default(), "{metadata_db}");
    }
}

/// Step v99 on an imported folder: `<docs>/profiles.json` that does not parse
/// falls back to the shipped layouts; valid JSON that is not an object,
/// `NaN` included, fails the migration at v99.
#[test]
fn import_reads_the_user_layout_file_like_v99() {
    let import_with = |content: Option<&[u8]>| {
        let dir = tempfile::tempdir().unwrap();
        write_folder(dir.path(), &old_state());
        if let Some(content) = content {
            std::fs::write(dir.path().join("profiles.json"), content).unwrap();
        }
        import_srs_folder(dir.path(), Platform::Windows)
    };
    let without_file = import_with(None).unwrap();
    assert!(!without_file.report.skipped_side_effects.iter().any(|e| e.starts_with("v99")));
    let shipped = without_file.configuration.shaker_layouts;
    for unreadable in [&b"{not json"[..], b"\xff\xfe{}", b"[1,]", b"\xef\xbb\xbf{}"] {
        let imported = import_with(Some(unreadable)).unwrap();
        assert_eq!(imported.configuration.shaker_layouts, shipped, "{unreadable:?}");
        assert!(imported.report.skipped_side_effects.contains(&"v99: delete <docs>/profiles.json".to_owned()));
    }
    for not_an_object in [&b"[1, 2]"[..], b" \"text\" ", b"5", b"NaN", b"null"] {
        let error = import_with(Some(not_an_object)).unwrap_err();
        assert!(matches!(&error, ImportError::Migration(e) if e.version == 99), "{not_an_object:?}: {error}");
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("profiles.json");
    std::fs::write(&path, b"{\"a\": \"\\udc00\"}").unwrap();
    let LegacyLayouts::Layouts(layouts) = read_legacy_layouts(&path).unwrap() else {
        panic!("native JSON object");
    };
    assert_eq!(layouts.get("a").unwrap().python_string().unwrap().codepoints(), &[0xdc00]);
}

/// An old Windows folder (template version 84, never started) goes through the chain with the Windows branches,
/// including the MOTIONBOX.model renames.
#[test]
fn imports_an_old_windows_folder() {
    let mut state = old_state();
    let motionbox = state.config.get_mut("MOTIONBOX").unwrap().as_dict_mut().unwrap();
    motionbox.insert("model", LegacyValue::Str("DEMO".into()));
    let dir = tempfile::tempdir().unwrap();
    write_folder(dir.path(), &state);
    let imported = import_srs_folder(dir.path(), Platform::Windows).unwrap();
    assert_eq!(imported.report.from_version, 84.into());
    let s = &imported.configuration.settings;
    assert_eq!(s.motion.model, "STR-SCN6150", "v275");
    assert_eq!(s.network_ports.get("dirt"), Some(20777), "not a first installation: template port kept");
    assert!(!s.features.gran_turismo, "v306 on Windows");
    assert!(s.belt.auto_search, "v408 only off Windows");
    assert_eq!(s.shakers.speaker_layout, "1 Speaker - Center", "template layout, no Linux first-install defaults");
    assert_eq!(imported.configuration.profiles.games().count(), 126);
    let ac = imported.configuration.profiles.get("ac").unwrap();
    assert_eq!(ac.shaker_engine_type, "O", "v271 for existing installations");
    assert_eq!(ac.motion_boost_factor, 35.0, "v153 first-install boost not applied");
    assert!(imported.configuration.joysticks.profile("Default").unwrap().is_empty(), "v325/v340 skip Windows");
    assert!(imported.report.skipped_side_effects.iter().any(|e| e.contains("registry")));
}

/// The required files must exist; a forbidden pickle global is rejected.
#[test]
fn import_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(import_srs_folder(dir.path(), Platform::Windows), Err(ImportError::MissingFile(_))));
    std::fs::write(dir.path().join("config.ini"), "[APP]\nconfig_file_version = 412\nfirst_time_installation = no\n")
        .unwrap();
    write_table(&dir.path().join("profile.db"), "unnamed", &[("ac", b"\x80\x02cos\nsystem\n.")]);
    assert!(matches!(import_srs_folder(dir.path(), Platform::Windows), Err(ImportError::Db(_))));
}

/// A game tune that no longer converts falls back to the factory tune; a bad vehicle is skipped.
#[test]
fn unconvertible_tunes_are_reported() {
    let mut state = fresh_state();
    let ac = state.profiles.get_mut("ac").unwrap().as_dict_mut().unwrap();
    ac.remove("haptic_effects");
    let api_vehicles = state.profiles.get_mut("api").unwrap().as_dict_mut().unwrap().get_mut("vehicles").unwrap();
    api_vehicles.as_dict_mut().unwrap().insert("Broken", LegacyValue::Int(3));
    let (configuration, warnings) = convert_state(&state);
    assert_eq!(configuration.profiles.get("ac").unwrap(), &{
        let mut f = crate::tunes::FactoryTunes::builtin().get("ac").unwrap().clone();
        f.vehicles.clear();
        f
    });
    assert!(warnings.iter().any(|w| w.location == "profile \"ac\"" && w.message.contains("factory tune used")));
    assert!(warnings.iter().any(|w| w.location.contains("vehicle \"Broken\"")));
    assert_eq!(configuration.profiles.get("api").unwrap().vehicles.len(), 4);
}

/// The first-run configuration saves and loads back unchanged; an empty directory loads first-run.
#[test]
fn configuration_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = crate::settings::paths::ConfigDir::at(dir.path().join("openrigcue"));
    let first = Configuration::first_run();
    assert_eq!(Configuration::load(&config_dir).unwrap(), first);
    first.save(&config_dir).unwrap();
    assert_eq!(Configuration::load(&config_dir).unwrap(), first);
    assert_eq!(Settings::load(&config_dir.settings_file()).unwrap(), Settings::default());
}

//! Tests of Tuning → SIM3D: its groups and writes, the feature rule of an effect and of its advanced rows,
//! the shortcut tooltip and the hot keys.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::Backend;
use crate::host::UiApp;
use crate::ui::{dispatch, navigate};

fn sim3d(backend: &std::sync::Arc<FakeBackend>, game: &str) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Sim3d, 0);
    app
}

fn stored(backend: &FakeBackend, path: &[&str]) -> serde_json::Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["tune"].clone(), |value, key| value[*key].clone())
}

#[test]
fn groups_and_every_value_follow_the_page() {
    let backend = FakeBackend::new();
    let mut app = sim3d(&backend, "acc");
    let html = main(&app);
    for group in ["Throttle", "Brake", "Clutch", "Seat", "Advanced Settings - Engine Haptics"] {
        assert!(html.contains(&format!(">{group}</option>")), "{group}");
    }
    assert!(html.contains("switch SIM3D off or to auto") && html.contains(">Downloaded profiles<"));
    let effect = &Registry::builtin().rumble_effects[0].id;
    for (arg, value, path, expected) in [
        (format!("SIM3D:{effect}"), "7", vec!["rumble_effects", effect.as_str()], json!(7)),
        (format!("SIM3D:{effect}:toggle"), "0", vec!["rumble_switch", effect.as_str()], json!(false)),
        ("SIM3D:rpm_start_rumble_perc".into(), "40", vec!["rumble_effects", "rpm_start_rumble"], json!(40)),
        ("SIM3D:rpm_fading_perc:high".into(), "90", vec!["rumble_effects", "rpm_stop_fading"], json!(90)),
        ("SIM3D:rpm_fading_perc".into(), "50", vec!["rumble_effects", "rpm_start_fading"], json!(50)),
        (
            "SIM3D:wheel_lock_min_activation_perc".into(),
            "35",
            vec!["rumble_effects", "wheel_lock_min_activation"],
            json!(35),
        ),
        (
            "SIM3D:traction_loss_min_activation_perc".into(),
            "25",
            vec!["rumble_effects", "traction_loss_min_activation"],
            json!(25),
        ),
    ] {
        dispatch(&mut app, "set", &arg, value).unwrap();
        assert_eq!(stored(&backend, &path), expected, "{arg}");
    }
    drop(app.take_threads());
}

#[test]
fn an_unused_kind_disables_its_effects_and_advanced_rows() {
    let backend = FakeBackend::new();
    let game = super::super::tuning::game_without("engine");
    let mut app = sim3d(&backend, &game);
    dispatch(&mut app, "section", "", "4").unwrap();
    let html = main(&app);
    assert!(html.contains("Not used by this game") && html.contains("is-disabled"), "engine rows");
    backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = json!(true);
    app.refresh_docs();
    assert!(!main(&app).contains("data-arg=\"SIM3D:rpm_fading_perc\""));
    assert!(!main(&app).contains("data-arg=\"SIM3D:throttle_engine\""));
    drop(app.take_threads());
}

#[test]
fn hot_keys_step_the_effects() {
    let backend = FakeBackend::new();
    let mut app = sim3d(&backend, "acc");
    let effect = Registry::builtin().rumble_effects[0].id.clone();
    dispatch(&mut app, "set", &format!("SIM3D:{effect}"), "4").unwrap();
    backend.docs.lock().unwrap()["ui_actions"] = json!([format!("rumble_{effect}_up")]);
    let pending = |action: &str| backend.ui_action_pending(action);
    assert!(apply_hotkeys(&mut app, &pending).unwrap());
    assert_eq!(stored(&backend, &["rumble_effects", &effect]), json!(5));
    drop(app.take_threads());
}

/// The opening tag of the element with `id` in `html`.
fn tag<'a>(html: &'a str, id: &str) -> &'a str {
    let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element {id}"));
    let start = html[..start].rfind('<').unwrap();
    let end = start + html[start..].find('>').unwrap();
    &html[start..=end]
}

/// The screen's body with the settings group `section` selected.
fn body(app: &mut MyApp, section: usize) -> String {
    app.ui.section = section;
    main(app)
}

#[test]
fn stored_effect_and_advanced_values_show_as_whole_numbers() {
    let backend = FakeBackend::new();
    let mut app = sim3d(&backend, "acc");
    let first = Registry::builtin().rumble_effects[0].id.clone();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["rumble_effects"][&first] = json!(7);
        docs["tune"]["rumble_switch"][&first] = json!("n");
        docs["tune"]["rumble_effects"]["seat_engine"] = json!(12);
        docs["tune"]["rumble_switch"]["seat_engine"] = json!(true);
        docs["tune"]["rumble_effects"]["rpm_start_fading"] = json!(63);
        docs["tune"]["rumble_effects"]["rpm_stop_fading"] = json!(91.0);
        docs["tune"]["rumble_effects"]["rpm_start_rumble"] = json!(20.7);
    }
    app.refresh_docs();
    let html = body(&mut app, 0);
    let row = tag(&html, &format!("c-sim3d-{first}"));
    assert!(row.contains("value=\"7\"") && row.contains(" disabled"), "{row}");
    assert!(!tag(&html, &format!("c-sim3d-{first}-toggle")).contains(" checked"), "the text n is off");
    let html = body(&mut app, 3);
    let seat = tag(&html, "c-sim3d-seat_engine");
    assert!(seat.contains("value=\"12\"") && !seat.contains(" disabled"));
    let html = body(&mut app, 4);
    assert!(tag(&html, "c-sim3d-rpm_fading_perc").contains("value=\"63\"") && html.contains(">63% - 91%<"));
    assert!(tag(&html, "c-sim3d-rpm_start_rumble_perc").contains("value=\"20\"") && html.contains(">20 %<"));
    drop(app.take_threads());
}

#[test]
fn the_groups_and_rows_follow_the_page_order() {
    let registry = Registry::builtin();
    let page = page();
    let titles: Vec<&str> = page.sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Throttle",
            "Brake",
            "Clutch",
            "Seat",
            "Advanced Settings - Engine Haptics",
            "Advanced Settings - Wheel Slip / Lock",
            "Advanced Settings - Traction Loss"
        ]
    );
    for (section, group) in page.sections.iter().zip(["throttle", "brake", "clutch", "seat"]) {
        let ids: Vec<&str> = section.controls.iter().map(|c| c.id.as_str()).collect();
        let expected: Vec<&str> =
            registry.rumble_effects.iter().filter(|e| e.group == group).map(|e| e.id.as_str()).collect();
        assert_eq!(ids, expected, "{group}");
    }
    let advanced: Vec<&str> =
        page.sections[4..].iter().flat_map(|s| s.controls.iter().map(|c| c.id.as_str())).collect();
    assert_eq!(
        advanced,
        [
            "rpm_start_rumble_perc",
            "rpm_fading_perc",
            "wheel_slip_min_activation_perc",
            "wheel_lock_min_activation_perc",
            "traction_loss_min_activation_perc"
        ]
    );
    assert_eq!(page.sections[..4].iter().map(|s| s.controls.len()).sum::<usize>(), 22);
    assert_eq!(registry.rumble_effects.len(), 22);
}

#[test]
fn malformed_values_show_and_are_not_rewritten_by_a_profile_load() {
    let backend = FakeBackend::new();
    let mut app = sim3d(&backend, "acc");
    let first = Registry::builtin().rumble_effects[0].id.clone();
    backend.docs.lock().unwrap()["tune"]["rumble_effects"]["rpm_start_fading"] = json!(63);
    for (table, key, value) in [
        ("rumble_effects", first.as_str(), serde_json::Value::Null),
        ("rumble_effects", first.as_str(), json!("x")),
        ("rumble_switch", first.as_str(), json!(2)),
        ("rumble_effects", "rpm_stop_fading", json!("95.5")),
        ("rumble_effects", "rpm_stop_fading", json!(" 95 ")),
    ] {
        backend.docs.lock().unwrap()["tune"][table][key] = value.clone();
        app.refresh_docs();
        backend.calls.lock().unwrap().clear();
        super::super::tuning::on_profile_load(&mut app);
        let rumble = backend
            .patches("profile")
            .into_iter()
            .filter(|p| p.get("rumble_effects").is_some() || p.get("rumble_switch").is_some())
            .count();
        assert_eq!(rumble, 0, "{table} {key} {value}");
        assert_eq!(stored(&backend, &[table, key]), value);
        for section in 0..7 {
            body(&mut app, section);
        }
    }
    assert!(body(&mut app, 4).contains(">63% - 95%<"), "number text shows as its number");
    drop(app.take_threads());
}

#[test]
fn the_value_conversions_of_the_service_documents_keep_their_rules() {
    use crate::backend::{native_bool, native_float, native_int_as_float, native_int_float};
    assert_eq!(native_bool(&LegacyValue::Bool(true)), Ok(true));
    assert_eq!(native_bool(&LegacyValue::from("No")), Ok(false));
    assert_eq!(native_bool(&LegacyValue::Int(1)), Ok(true));
    assert!(native_bool(&LegacyValue::Float(1.0)).is_err());
    assert!(native_bool(&LegacyValue::None).is_err());
    assert_eq!(native_float(&LegacyValue::from(" 7 ")).unwrap(), 7.0);
    assert!(native_float(&LegacyValue::None).is_err());
    assert!(native_int_as_float(&LegacyValue::from("95.5")).is_err());
    assert_eq!(native_int_float(&LegacyValue::from("70.6")).unwrap(), 70.0);
    assert!(LegacyValue::from("no").is_truthy());
}

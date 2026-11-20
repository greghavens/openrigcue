//! Tests of Tuning → HapticR: offered only while enabled, its writes and hot keys, the engine rows' feature
//! rule, and the defaults of a loaded profile.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::Backend;
use crate::host::UiApp;
use crate::ui::{dispatch, navigate};

fn hapticr(backend: &std::sync::Arc<FakeBackend>, enabled: bool) -> MyApp {
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("acc");
        docs["settings"]["app"]["enable_hapticr"] = json!(enabled);
    }
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::HapticR, 0);
    app
}

fn stored(backend: &FakeBackend, path: &[&str]) -> serde_json::Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["tune"].clone(), |value, key| value[*key].clone())
}

#[test]
fn hapticr_is_offered_only_while_enabled() {
    let backend = FakeBackend::new();
    let app = hapticr(&backend, false);
    assert_ne!(app.ui.screen, Screen::HapticR, "a disabled HapticR opens its section's start");
    assert!(!Screen::of_group(crate::ui::Group::Tuning, &app).contains(&Screen::HapticR));
    let mut app = app;
    backend.docs.lock().unwrap()["ui_actions"] = json!([format!("haptic_{}_up", haptic_effects()[0].id)]);
    let pending = |action: &str| backend.ui_action_pending(action);
    assert!(!apply_hotkeys(&mut app, &pending).unwrap(), "no hot keys while disabled");
    drop(app.take_threads());
}

#[test]
fn values_and_hot_keys_are_stored_in_the_haptic_tables() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, true);
    assert_eq!(app.ui.screen, Screen::HapticR);
    let html = main(&app);
    assert!(html.contains("switch HapticR off or to auto") && html.contains(">Wheels</option>"));
    let effect = haptic_effects()[0].id.clone();
    dispatch(&mut app, "set", &format!("HapticR:{effect}"), "6").unwrap();
    assert_eq!(stored(&backend, &["haptic_effects", &effect]), json!(6));
    dispatch(&mut app, "set", &format!("HapticR:{effect}:toggle"), "1").unwrap();
    assert_eq!(stored(&backend, &["haptic_switch", &effect]), json!(true));
    dispatch(&mut app, "set", "HapticR:rpm_start_haptic_perc", "33").unwrap();
    assert_eq!(stored(&backend, &["haptic_effects", "rpm_start_rumble"]), json!(33));
    backend.docs.lock().unwrap()["ui_actions"] = json!([format!("haptic_{effect}_down")]);
    let pending = |action: &str| backend.ui_action_pending(action);
    assert!(apply_hotkeys(&mut app, &pending).unwrap());
    assert_eq!(stored(&backend, &["haptic_effects", &effect]), json!(5));
    drop(app.take_threads());
}

#[test]
fn the_rpm_rows_follow_the_engine_feature() {
    let backend = FakeBackend::new();
    let game = super::super::tuning::game_without("engine");
    backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = json!(true);
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::HapticR, 0);
    let html = main(&app);
    assert!(!html.contains("Advanced Settings - Engine Haptics</option>"), "both engine rows are hidden");
    drop(app.take_threads());
}

#[test]
fn a_loaded_profile_takes_the_defaults_for_missing_values() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, true);
    let effect = haptic_effects()[0].id.clone();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["haptic_effects"] = json!({"rpm_stop_fading": 90});
        docs["tune"]["haptic_switch"] = json!({});
    }
    app.refresh_docs();
    super::super::reload::profile_load(&mut app).unwrap();
    // Each missing effect empties its table again, so the last one keeps its 0 and on.
    let last = haptic_effects().last().unwrap().id.clone();
    assert_eq!(stored(&backend, &["haptic_effects", &last]), json!(0));
    assert_eq!(stored(&backend, &["haptic_switch", &last]), json!(true));
    if effect != last {
        assert_eq!(stored(&backend, &["haptic_effects", &effect]), serde_json::Value::Null);
        assert_eq!(stored(&backend, &["haptic_switch", &effect]), serde_json::Value::Null);
    }
    // A missing effect empties the table, its fade stop included: the range takes 94 – 100.
    assert_eq!(stored(&backend, &["haptic_effects", "rpm_start_fading"]), json!(94));
    assert_eq!(stored(&backend, &["haptic_effects", "rpm_stop_fading"]), json!(100));
    assert_eq!(stored(&backend, &["haptic_effects", "wheel_lock_min_activation"]), json!(50));
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
fn hapticr_is_offered_for_boolean_words_only() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, true);
    for (value, offered) in [
        (json!("y"), true),
        (json!(true), true),
        (json!("on"), false),
        (json!(1.0), false),
        (serde_json::Value::Null, false),
        (json!(false), false),
    ] {
        backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = value.clone();
        app.refresh_docs();
        assert_eq!(enabled(&app), offered, "{value}");
        assert_eq!(Screen::of_group(crate::ui::Group::Tuning, &app).contains(&Screen::HapticR), offered, "{value}");
    }
    drop(app.take_threads());
}

#[test]
fn a_disabled_hapticr_leaves_a_loaded_profile_alone() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, false);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["haptic_effects"] = json!({});
        docs["tune"]["haptic_switch"] = json!({});
    }
    app.refresh_docs();
    super::super::reload::profile_load(&mut app).unwrap();
    assert_eq!(stored(&backend, &["haptic_effects"]), json!({}));
    assert_eq!(stored(&backend, &["haptic_switch"]), json!({}));
    drop(app.take_threads());
}

#[test]
fn stored_effect_and_advanced_values_show_as_whole_numbers() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, true);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["haptic_effects"]["wheel_gear"] = json!(7);
        docs["tune"]["haptic_switch"]["wheel_gear"] = json!(false);
        docs["tune"]["haptic_switch"]["wheel_heave"] = json!(true);
        docs["tune"]["haptic_effects"]["rpm_start_fading"] = json!("70.6");
        docs["tune"]["haptic_effects"]["rpm_stop_fading"] = json!(95.9);
        docs["tune"]["haptic_effects"]["rpm_start_rumble"] = json!(12.7);
        docs["tune"]["haptic_effects"]["wheel_slip_min_activation"] = json!(20);
    }
    app.refresh_docs();
    let html = body(&mut app, 0);
    let gear = tag(&html, "c-hapticr-wheel_gear");
    assert!(gear.contains("value=\"7\"") && gear.contains(" disabled"), "{gear}");
    assert!(!tag(&html, "c-hapticr-wheel_gear-toggle").contains(" checked"));
    assert!(!tag(&html, "c-hapticr-wheel_heave").contains(" disabled"));
    let html = body(&mut app, 1);
    assert!(tag(&html, "c-hapticr-rpm_start_haptic_perc").contains("value=\"12\"") && html.contains(">12 %<"));
    assert!(tag(&html, "c-hapticr-rpm_fading_perc").contains("value=\"70\""));
    assert!(tag(&html, "c-hapticr-rpm_fading_perc-high").contains("value=\"95\"") && html.contains(">70% - 95%<"));
    assert!(tag(&body(&mut app, 2), "c-hapticr-wheel_slip_min_activation_perc").contains("value=\"20\""));
    // Another stored fade range.
    backend.docs.lock().unwrap()["tune"]["haptic_effects"]["rpm_start_fading"] = json!(63);
    backend.docs.lock().unwrap()["tune"]["haptic_effects"]["rpm_stop_fading"] = json!(91);
    app.refresh_docs();
    assert!(body(&mut app, 1).contains(">63% - 91%<"));
    drop(app.take_threads());
}

#[test]
fn effects_the_game_does_not_use_are_disabled_or_hidden_with_their_thresholds() {
    let backend = FakeBackend::new();
    let game = super::super::tuning::game_without("wheel_slip");
    let mut app = hapticr(&backend, true);
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = json!(false);
    app.refresh_docs();
    let html = body(&mut app, 0);
    assert!(html.contains("data-control=\"wheel_wheel_slip\"") && html.contains("Not used by this game"), "{game}");
    assert!(tag(&html, "c-hapticr-wheel_wheel_slip").contains(" disabled"));
    assert!(tag(&body(&mut app, 2), "c-hapticr-wheel_slip_min_activation_perc").contains(" disabled"));
    // The setting as it is now: hidden.
    backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = json!(true);
    let html = body(&mut app, 0);
    assert!(!html.contains("data-control=\"wheel_wheel_slip\""));
    assert!(!html.contains("Advanced Settings - Wheel Slip / Lock</option>"), "both thresholds are hidden");
    drop(app.take_threads());
}

#[test]
fn the_shortcuts_action_lists_the_switch_then_every_step_up_then_down() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, true);
    let header = page()
        .header
        .iter()
        .position(|b| matches!(b, crate::spec::Widget::Button { run: crate::spec::Action::Shortcuts { .. }, .. }))
        .unwrap();
    dispatch(&mut app, "action", &format!("HapticR:@header:{header}"), "").unwrap();
    let Some(crate::ui::dialogs::Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("the shortcut editor") };
    let keys: Vec<&str> = editor.actions.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys.len(), 19);
    assert_eq!(keys[0], "haptic_switch");
    assert_eq!(keys[1], "haptic_wheel_wheel_slip_up");
    assert_eq!(keys[9], "haptic_wheel_engine_up");
    assert_eq!(keys[10], "haptic_wheel_wheel_slip_down");
    assert_eq!(keys[18], "haptic_wheel_engine_down");
    drop(app.take_threads());
}

#[test]
fn the_rows_follow_the_page_order() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, true);
    let sections: Vec<(String, Vec<String>)> =
        page().sections.iter().map(|s| (s.title.clone(), s.controls.iter().map(|c| c.id.clone()).collect())).collect();
    let expected = [
        (
            "Wheels",
            vec![
                "wheel_wheel_slip",
                "wheel_abs_lock",
                "wheel_gear",
                "wheel_heave",
                "wheel_rumble_strips",
                "wheel_surge_acceleration",
                "wheel_surge_deceleration",
                "wheel_traction_loss",
                "wheel_engine",
            ],
        ),
        ("Advanced Settings - Engine Haptics", vec!["rpm_start_haptic_perc", "rpm_fading_perc"]),
        (
            "Advanced Settings - Wheel Slip / Lock",
            vec!["wheel_slip_min_activation_perc", "wheel_lock_min_activation_perc"],
        ),
        ("Advanced Settings - Traction Loss", vec!["traction_loss_min_activation_perc"]),
    ];
    assert_eq!(sections.len(), expected.len());
    for ((title, ids), (want_title, want_ids)) in sections.iter().zip(expected) {
        assert_eq!((title.as_str(), ids.iter().map(String::as_str).collect::<Vec<_>>()), (want_title, want_ids));
    }
    let html = body(&mut app, 0);
    let positions: Vec<usize> =
        sections[0].1.iter().map(|id| html.find(&format!("data-control=\"{id}\"")).unwrap()).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "the screen shows them in that order");
    drop(app.take_threads());
}

#[test]
fn the_value_conversions_of_the_service_documents_keep_their_rules() {
    use crate::backend::{native_bool, native_int_as_float, native_int_float};
    use openrigcue_model::native_value::LegacyValue;
    assert_eq!(native_int_as_float(&LegacyValue::from("95")).unwrap(), 95.0);
    assert!(native_int_as_float(&LegacyValue::from("95.5")).is_err());
    assert_eq!(native_int_as_float(&LegacyValue::Float(95.9)).unwrap(), 95.0);
    assert_eq!(native_int_float(&LegacyValue::from(" 70.6 ")).unwrap(), 70.0);
    assert!(native_int_float(&LegacyValue::from("inf")).is_err());
    assert!(native_int_float(&LegacyValue::None).is_err());
    assert!(!LegacyValue::from("").is_truthy());
    assert!(LegacyValue::from("no").is_truthy());
    assert_eq!(native_bool(&LegacyValue::from("Y")), Ok(true));
    assert_eq!(native_bool(&LegacyValue::Int(1)), Ok(true));
    assert_eq!(native_bool(&LegacyValue::Int(0)), Ok(false));
    assert!(native_bool(&LegacyValue::Float(1.0)).is_err());
    assert!(native_bool(&LegacyValue::from("on")).is_err());
    assert!(native_bool(&LegacyValue::None).is_err());
}

#[test]
fn effects_and_the_fade_range_are_stored_as_whole_numbers_in_the_haptic_table() {
    let backend = FakeBackend::new();
    let mut app = hapticr(&backend, true);
    dispatch(&mut app, "set", "HapticR:wheel_gear", "7").unwrap();
    assert!(backend.patches("profile").contains(&json!({"haptic_effects": {"wheel_gear": 7}})));
    dispatch(&mut app, "set", "HapticR:wheel_gear:toggle", "0").unwrap();
    assert!(backend.patches("profile").contains(&json!({"haptic_switch": {"wheel_gear": false}})));
    dispatch(&mut app, "set", "HapticR:rpm_fading_perc", "60").unwrap();
    dispatch(&mut app, "set", "HapticR:rpm_fading_perc:high", "90").unwrap();
    assert!(backend.patches("profile").contains(&json!({"haptic_effects": {"rpm_stop_fading": 90}})));
    assert!(backend.patches("profile").contains(&json!({"haptic_effects": {"rpm_start_fading": 60}})));
    app.ui.section = 1;
    assert!(main(&app).contains(">60% - 90%<"));
    drop(app.take_threads());
}

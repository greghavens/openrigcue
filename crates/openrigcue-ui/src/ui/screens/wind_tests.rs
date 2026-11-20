//! Tests of Tuning → Wind: its rows, every value it stores, the fan-count, game and stand-by rules, its
//! buttons and the rule a loaded profile follows for one fan.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::host::UiApp;
use crate::ui::dialogs::Dialog;
use crate::ui::{dispatch, navigate};

/// A flight game of the catalogue (the ejection seat shows for it).
fn flight_game() -> String {
    openrigcue_config::catalog::Catalog::builtin()
        .games()
        .iter()
        .find(|g| g.kind != openrigcue_config::catalog::GameKind::Racing)
        .map(|g| g.id.clone())
        .expect("a game that is not a racing game")
}

/// The app on the Wind screen with `game` running.
fn wind(backend: &std::sync::Arc<FakeBackend>, game: &str) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Wind, 0);
    app
}

fn stored(backend: &FakeBackend, key: &str) -> serde_json::Value {
    backend.docs.lock().unwrap()["tune"][key].clone()
}

#[test]
fn every_group_shows_its_rows_and_the_actions() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, "acc");
    let html = main(&app);
    for group in ["Left / Right", "Center", "Max Speed", "Wind Curving", "Speed"] {
        assert!(html.contains(&format!(">{group}</option>")), "{group}");
    }
    assert!(!html.contains(">Special Effects</option>"), "racing games have no ejection seat");
    assert!(
        html.contains("data-arg=\"Wind:fan_power_perc\"") && html.contains("data-arg=\"Wind:fan_power_perc:high\"")
    );
    assert!(html.contains(">90%<"), "the actual power is the stored maximum");
    assert!(html.contains(">Lower-power shortcut<") && html.contains(">Raise-power shortcut<"));
    assert!(html.contains(">Actions<") && html.contains(">Shortcuts<") && html.contains(">Hardware setup<"));
    dispatch(&mut app, "section", "", "2").unwrap();
    assert!(main(&app).contains("Km/h"));
    drop(app.take_threads());
}

#[test]
fn every_wind_value_is_stored_as_the_page_stores_it() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, &flight_game());
    for (arg, value, key, expected) in [
        ("Wind:fan_power_perc", "20", "power_wind_min_speed_perc", json!(20)),
        ("Wind:fan_power_perc:high", "80", "power_wind_max_speed_perc", json!(80)),
        ("Wind:fan_power_perc", "95", "power_wind_min_speed_perc", json!(80)),
        ("Wind:fan_power_center_perc:high", "60", "power_center_wind_max_speed_perc", json!(60)),
        ("Wind:fan_power_center_perc", "10", "power_center_wind_min_speed_perc", json!(10)),
        ("Wind:default_max_speed", "250", "default_max_speed_kmh", json!(250)),
        ("Wind:wind_curving", "0", "wind_curving", json!(0)),
        ("Wind:curving_effect", "7", "curving_effect", json!(7)),
        ("Wind:speed_factor", "5", "speed_factor", json!(5)),
        ("Wind:ejection_seat", "1", "enable_ejection_seat", json!(1)),
    ] {
        dispatch(&mut app, "set", arg, value).unwrap();
        assert_eq!(stored(&backend, key), expected, "{arg} {value}");
    }
    assert!(main(&app).contains(">Special Effects</option>"), "a flight game has the ejection seat");
    drop(app.take_threads());
}

#[test]
fn the_maximum_speed_follows_the_measurement_unit() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, "acc");
    backend.docs.lock().unwrap()["settings"]["app"]["speed_unit"] = json!("mph");
    app.refresh_docs();
    dispatch(&mut app, "section", "", "2").unwrap();
    let html = main(&app);
    assert!(html.contains("124Mph"), "200 km/h shows as 124 mph");
    dispatch(&mut app, "set", "Wind:default_max_speed", "100").unwrap();
    assert_eq!(stored(&backend, "default_max_speed_kmh"), json!(160));
    drop(app.take_threads());
}

#[test]
fn wind_curving_needs_two_fans_and_a_loaded_profile_follows_one_fan() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"] = json!("1");
    let mut app = wind(&backend, "acc");
    dispatch(&mut app, "section", "", "3").unwrap();
    let html = main(&app);
    assert!(html.contains("Needs 2 fans (Hardware)") && html.contains("is-disabled"));
    assert_eq!(stored(&backend, "wind_curving"), json!(0), "the profile load switched curving off");
    assert_eq!(stored(&backend, "curving_effect"), json!(1));
    drop(app.take_threads());
}

#[test]
fn stand_by_locks_the_rows_unless_a_fan_is_always_on() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Wind, 0);
    assert!(main(&app).contains("Tuning is unavailable in Stand By mode"));
    dispatch(&mut app, "set", "Wind:fan_power_perc", "30").unwrap();
    assert_eq!(stored(&backend, "power_wind_min_speed_perc").as_f64(), Some(10.0), "nothing is written while locked");
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["wind"] = json!(2);
    app.refresh_docs();
    assert!(!main(&app).contains("Tuning is unavailable in Stand By mode"));
    dispatch(&mut app, "set", "Wind:fan_power_perc", "30").unwrap();
    assert_eq!(stored(&backend, "power_wind_min_speed_perc"), json!(30));
    drop(app.take_threads());
}

#[test]
fn buttons_open_the_shortcut_editor_and_hardware() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, "acc");
    dispatch(&mut app, "action", "Wind:fan_actual_perc:0", "").unwrap();
    let Some(Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("shortcut editor") };
    assert_eq!(editor.actions[0].0, "power_wind_down");
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    let header =
        page().header.iter().position(|b| matches!(b, Widget::Button { text, .. } if text == "Hardware setup"));
    dispatch(&mut app, "action", &format!("Wind:@header:{}", header.unwrap()), "").unwrap();
    assert_eq!(app.ui.screen, Screen::Hardware);
    drop(app.take_threads());
}

#[test]
fn a_unit_change_shows_the_max_speed_in_the_new_unit_and_stores_it_through_the_slider() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["default_max_speed_kmh"] = json!(400);
    app.refresh_docs();
    // km/h → mph: 400 km/h shows as 248 mph, stored again as 248 × 1.60934 = 399 km/h.
    dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["speed_unit"], json!("mph"));
    assert_eq!(stored(&backend, "default_max_speed_kmh"), json!(399));
    navigate(&mut app, Screen::Wind, 2);
    assert!(main(&app).contains("247Mph"), "399 km/h shows as 247 mph");
    // The same unit again moves nothing.
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap();
    assert!(backend.patches("profile").is_empty());
    // mph → km/h: the stored km/h show and are stored as whole km/h.
    dispatch(&mut app, "set", "App:measurement_unit", "0").unwrap();
    assert_eq!(stored(&backend, "default_max_speed_kmh"), json!(399));
    assert!(main(&app).contains("399Km/h"));
    // A profile without the value: the unit is stored, the max speed reports the missing key.
    backend.docs.lock().unwrap()["tune"].as_object_mut().unwrap().remove("default_max_speed_kmh");
    app.refresh_docs();
    let error = units_changed(&mut app, true).unwrap_err();
    assert!(error.contains("default_max_speed_kmh"), "{error}");
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
fn the_actual_power_shows_each_stored_maximum_on_its_own() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["power_wind_max_speed_perc"] = json!(35);
    backend.docs.lock().unwrap()["tune"]["power_center_wind_max_speed_perc"] = json!(17);
    app.refresh_docs();
    assert!(body(&mut app, 0).contains("id=\"c-wind-fan_actual_perc\">35%<"));
    assert!(body(&mut app, 1).contains("id=\"c-wind-fan_center_actual_perc\">17%<"));
    for (invalid, shown) in [
        (serde_json::Value::Null, "0%"),
        (json!("1.5"), "1%"),
        (json!([]), "0%"),
        (json!({}), "0%"),
        (json!(3.8), "3%"),
        (json!("bad"), "0%"),
        (json!(39), "39%"),
    ] {
        backend.docs.lock().unwrap()["tune"]["power_wind_max_speed_perc"] = invalid.clone();
        backend.docs.lock().unwrap()["tune"]["power_center_wind_max_speed_perc"] = json!(36);
        app.refresh_docs();
        assert!(body(&mut app, 0).contains(&format!("id=\"c-wind-fan_actual_perc\">{shown}<")), "{invalid}");
        assert!(body(&mut app, 1).contains("id=\"c-wind-fan_center_actual_perc\">36%<"), "{invalid}");
    }
    drop(app.take_threads());
}

#[test]
fn stored_speed_curving_range_and_ejection_values_show() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, &flight_game());
    {
        let mut docs = backend.docs.lock().unwrap();
        for (key, value) in [
            ("default_max_speed_kmh", 347),
            ("speed_factor", 4),
            ("enable_ejection_seat", 1),
            ("wind_curving", 1),
            ("curving_effect", 6),
            ("power_wind_min_speed_perc", 23),
            ("power_wind_max_speed_perc", 76),
            ("power_center_wind_min_speed_perc", 14),
            ("power_center_wind_max_speed_perc", 83),
        ] {
            docs["tune"][key] = json!(value);
        }
    }
    app.refresh_docs();
    let html = body(&mut app, 0);
    assert!(tag(&html, "c-wind-fan_power_perc").contains("value=\"23\""));
    assert!(tag(&html, "c-wind-fan_power_perc-high").contains("value=\"76\"") && html.contains(">23% - 76%<"));
    let html = body(&mut app, 1);
    assert!(tag(&html, "c-wind-fan_power_center_perc").contains("value=\"14\""));
    assert!(tag(&html, "c-wind-fan_power_center_perc-high").contains("value=\"83\""));
    assert!(tag(&body(&mut app, 2), "c-wind-default_max_speed").contains("value=\"347\""));
    let html = body(&mut app, 3);
    assert!(tag(&html, "c-wind-wind_curving").contains(" checked"));
    assert!(tag(&html, "c-wind-curving_effect").contains("value=\"6\""));
    assert!(tag(&body(&mut app, 4), "c-wind-speed_factor").contains("value=\"4\""));
    assert!(tag(&body(&mut app, 5), "c-wind-ejection_seat").contains(" checked"));
    drop(app.take_threads());
}

#[test]
fn the_maximum_speed_shows_stored_numbers_and_number_text_in_the_chosen_unit() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, "acc");
    for (unit, stored, value, shown) in [
        ("mph", json!(347), "215", "215Mph"),
        ("kmh", json!(347), "347", "347Km/h"),
        ("kmh", json!("200"), "200", "200Km/h"),
        ("mph", json!("200"), "124", "124Mph"),
        ("kmh", serde_json::Value::Null, "0", "0Km/h"),
    ] {
        backend.docs.lock().unwrap()["settings"]["app"]["speed_unit"] = json!(unit);
        backend.docs.lock().unwrap()["tune"]["default_max_speed_kmh"] = stored.clone();
        app.refresh_docs();
        let html = body(&mut app, 2);
        assert!(html.contains(&format!(">{shown}<")), "{unit} {stored}");
        assert!(tag(&html, "c-wind-default_max_speed").contains(&format!("value=\"{value}\"")), "{unit} {stored}");
    }
    // A factor that is no number does not keep the maximum speed from showing.
    backend.docs.lock().unwrap()["settings"]["app"]["speed_unit"] = json!("kmh");
    backend.docs.lock().unwrap()["tune"]["default_max_speed_kmh"] = json!(245);
    backend.docs.lock().unwrap()["tune"]["speed_factor"] = json!("bad");
    app.refresh_docs();
    assert!(tag(&body(&mut app, 2), "c-wind-default_max_speed").contains("value=\"245\""));
    assert!(tag(&body(&mut app, 4), "c-wind-speed_factor").contains("value=\"0\""));
    drop(app.take_threads());
}

#[test]
fn stored_switch_values_use_the_boolean_grammar() {
    use crate::backend::python_bool;
    for value in [json!("y"), json!("TRUE"), json!(1), json!(true)] {
        assert_eq!(python_bool(&value), Ok(true));
    }
    for value in [json!("n"), json!("F"), json!(0), json!(false)] {
        assert_eq!(python_bool(&value), Ok(false));
    }
    for value in [json!("on"), json!(" yes"), json!(2), json!(1.0), serde_json::Value::Null, json!([])] {
        assert!(python_bool(&value).is_err(), "{value}");
    }
}

#[test]
fn drawing_the_wind_screen_asks_the_service_nothing() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, "acc");
    backend.calls.lock().unwrap().clear();
    for section in 0..6 {
        body(&mut app, section);
    }
    let calls = backend.calls.lock().unwrap();
    assert!(!calls.iter().any(|(m, p, _)| *m == crate::backend::Method::Get && p == "profile"), "{calls:?}");
    drop(calls);
    drop(app.take_threads());
}

#[test]
fn a_profile_without_a_centre_range_or_ejection_seat_gets_the_full_range_and_off() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, &flight_game());
    {
        let mut docs = backend.docs.lock().unwrap();
        let tune = docs["tune"].as_object_mut().unwrap();
        tune.remove("power_center_wind_min_speed_perc");
        tune.insert("power_center_wind_max_speed_perc".into(), serde_json::Value::Null);
        tune.remove("enable_ejection_seat");
    }
    app.refresh_docs();
    super::super::reload::profile_load(&mut app).unwrap();
    assert_eq!(stored(&backend, "power_center_wind_min_speed_perc"), json!(0));
    assert_eq!(stored(&backend, "power_center_wind_max_speed_perc"), json!(100));
    assert_eq!(stored(&backend, "enable_ejection_seat"), json!(0));
    assert!(body(&mut app, 1).contains(">0% - 100%<"));
    assert!(!tag(&body(&mut app, 5), "c-wind-ejection_seat").contains(" checked"));
    drop(app.take_threads());
}

#[test]
fn a_malformed_belt_value_does_not_keep_the_wind_values_from_showing() {
    let backend = FakeBackend::new();
    let mut app = wind(&backend, &flight_game());
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["default_max_speed_kmh"] = json!(245);
        docs["tune"]["belt_surge_enabled"] = json!("on");
        docs["tune"]["enable_ejection_seat"] = json!(1);
    }
    app.refresh_docs();
    super::super::tuning::on_profile_load(&mut app);
    assert_eq!(stored(&backend, "belt_surge_enabled"), json!("on"));
    assert_eq!(stored(&backend, "enable_ejection_seat"), json!(1));
    assert!(tag(&body(&mut app, 2), "c-wind-default_max_speed").contains("value=\"245\""));
    assert!(tag(&body(&mut app, 5), "c-wind-ejection_seat").contains(" checked"));
    drop(app.take_threads());
}

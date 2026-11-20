//! Tests of Settings → App: every group's rows, every value it stores, the launcher switches, the factory
//! reset with its confirmation, the mobile companion, the startup page, the Wind max speed a unit change
//! stores, and the ways into Settings → Help.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::Method;
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Region};

fn on_app(backend: &std::sync::Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::App, 0);
    drop(app.take_threads());
    app
}

fn stored(backend: &FakeBackend, key: &str) -> serde_json::Value {
    backend.docs.lock().unwrap()["settings"]["app"][key].clone()
}

/// The index of a group of the page, by title.
fn group(title: &str) -> String {
    page().sections.iter().position(|section| section.title == title).unwrap().to_string()
}

#[test]
fn every_group_is_offered_and_every_row_is_editable() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    let general = main(&app);
    assert!(general.contains(">Application<") || general.contains(">APPLICATION<"), "{general}");
    for title in [
        "General",
        "Startup Options",
        "Alerts / Notifications",
        "Network",
        "Logging",
        "Measurement",
        "Reset",
        "Credits",
    ] {
        assert!(general.contains(&format!(">{title}</option>")), "{title}");
    }
    assert!(general.contains(env!("CARGO_PKG_VERSION")) && general.contains("data-arg=\"App:theme\""));
    for (title, controls) in [
        (
            "Startup Options",
            vec!["startup_page", "x_button_exit", "start_minimized", "start_with_login", "show_splash_screen"],
        ),
        ("Alerts / Notifications", vec!["voice_notifications", "shift_sound"]),
        ("Logging", vec!["log_level"]),
        ("Measurement", vec!["measurement_unit"]),
        ("Reset", vec!["reset"]),
    ] {
        dispatch(&mut app, "section", "", &group(title)).unwrap();
        let html = main(&app);
        assert!(!html.contains(" disabled"), "{title} rows are editable: {html}");
        for control in controls {
            assert!(html.contains(&format!("data-arg=\"App:{control}")), "{title}: {control}");
        }
    }
    // Five start-up rows: four shown, the last under More settings.
    dispatch(&mut app, "section", "", &group("Startup Options")).unwrap();
    assert!(main(&app).contains("More settings · 1"));
    dispatch(&mut app, "section", "", &group("Network")).unwrap();
    let network = main(&app);
    assert!(network.contains("127.0.0.1") && network.contains("data-act=\"companion\""));
    dispatch(&mut app, "section", "", &group("Credits")).unwrap();
    assert!(main(&app).contains("Made with FMOD Studio by Firelight Technologies Pty Ltd."));
    drop(app.take_threads());
}

#[test]
fn every_app_setting_is_stored_under_its_settings_name() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    let startup = page().control("startup_page").cloned().unwrap();
    let Widget::Select { options } = &startup.widget else { panic!("startup page select") };
    let motion = options.iter().position(|choice| choice.value == json!("Motion")).unwrap();
    for (arg, value, key, expected) in [
        ("App:startup_page", motion.to_string(), "start_page", json!("Motion")),
        ("App:x_button_exit", "0".into(), "close_button_quits", json!(false)),
        ("App:x_button_exit", "1".into(), "close_button_quits", json!(true)),
        ("App:start_minimized", "1".into(), "start_minimized", json!(true)),
        ("App:voice_notifications", "1".into(), "voice_notifications", json!(true)),
        ("App:shift_sound", "1".into(), "gear_shift_sound", json!(true)),
        ("App:shift_sound", "0".into(), "gear_shift_sound", json!(false)),
        ("App:log_level", "4".into(), "log_level", json!(10)),
        ("App:log_level", "0".into(), "log_level", json!(50)),
        ("App:measurement_unit", "1".into(), "speed_unit", json!("mph")),
        ("App:measurement_unit", "0".into(), "speed_unit", json!("kmh")),
    ] {
        dispatch(&mut app, "set", arg, &value).unwrap();
        assert_eq!(stored(&backend, key), expected, "{arg} {value}");
        assert_eq!(app.setting(&["app", key]), &expected, "the screen reads the stored value: {arg}");
    }
    // A position outside the choices stores nothing.
    let before = stored(&backend, "log_level");
    assert!(dispatch(&mut app, "set", "App:log_level", "9").is_err());
    assert_eq!(stored(&backend, "log_level"), before);
    drop(app.take_threads());
}

#[test]
fn the_shown_choices_follow_the_stored_values() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["app"]["log_level"] = json!(7);
        docs["settings"]["app"]["start_page"] = json!("Belt");
    }
    let mut app = on_app(&backend);
    dispatch(&mut app, "section", "", &group("Logging")).unwrap();
    assert!(main(&app).contains("<option value=\"4\" selected>Debug</option>"), "an unknown level shows as Debug");
    dispatch(&mut app, "section", "", &group("Startup Options")).unwrap();
    assert!(main(&app).contains("selected>Belt</option>"));
    dispatch(&mut app, "section", "", &group("Measurement")).unwrap();
    assert!(main(&app).contains("selected>km/h</option>"));
    drop(app.take_threads());
}

#[test]
fn the_launcher_switches_store_their_flags() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    dispatch(&mut app, "set", "App:start_with_login", "0").unwrap();
    dispatch(&mut app, "set", "App:show_splash_screen", "0").unwrap();
    let patches = backend.patches("settings");
    assert!(patches.iter().any(|p| p == &json!({"app": {"start_with_login": false}})), "{patches:?}");
    assert!(patches.iter().any(|p| p == &json!({"app": {"show_splash_screen": false}})), "{patches:?}");
    assert_eq!(app.setting(&["app", "start_with_login"]), &json!(false));
    dispatch(&mut app, "section", "", &group("Startup Options")).unwrap();
    let html = main(&app);
    assert!(!html.contains("data-arg=\"App:start_with_login\" checked"), "{html}");
    // A refused write leaves the switch as stored and is not an error.
    backend.docs.lock().unwrap()["settings_failure"] = json!("PermissionError");
    assert_eq!(dispatch(&mut app, "set", "App:start_with_login", "1"), Ok(()));
    assert_eq!(app.setting(&["app", "start_with_login"]), &json!(false));
    drop(app.take_threads());
}

#[test]
fn the_theme_choice_switches_the_frame() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    let apex = Theme::ALL.iter().position(|theme| theme.id() == "apex").unwrap();
    dispatch(&mut app, "set", "App:theme", &apex.to_string()).unwrap();
    assert_eq!(app.ui.theme.id(), "apex");
    assert_eq!(stored(&backend, "theme"), json!("apex"));
    assert_eq!(app.ui.screen, Screen::App, "the screen stays");
    drop(app.take_threads());
}

#[test]
fn factory_reset_asks_first_and_then_calls_the_service() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    dispatch(&mut app, "section", "", &group("Reset")).unwrap();
    assert!(main(&app).contains("Restore Factory Settings"));
    dispatch(&mut app, "action", "App:reset:", "").unwrap();
    assert_eq!(app.ui.dialog, Some(Dialog::Settings(SettingsDialog::FactoryReset)));
    let dialog = crate::ui::render(&app, Region::Dialog);
    assert!(dialog.contains("Return to Factory Settings") && dialog.contains("erases your tuning profiles"));
    let resets = |backend: &FakeBackend| {
        backend.calls.lock().unwrap().iter().filter(|(m, p, _)| *m == Method::Post && p == "factory-reset").count()
    };
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert_eq!(resets(&backend), 0, "Cancel keeps everything");
    dispatch(&mut app, "action", "App:reset:", "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    assert_eq!(resets(&backend), 1);
    drop(app.take_threads());
}

#[test]
fn the_network_group_opens_the_mobile_companion() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    dispatch(&mut app, "section", "", &group("Network")).unwrap();
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "companion", "", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "mobile-companion"));
    drop(app.take_threads());
}

#[test]
fn a_mobile_companion_failure_is_a_translated_alert_that_is_not_spoken() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["mobile_companion_failure"] = json!("The companion server could not start");
        docs["translations"] = json!({"The companion server could not start": "Der Begleitserver startete nicht"});
    }
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "companion", "", "").unwrap();
    let notice = app.ui.notices.history.last().expect("the failure is shown");
    assert_eq!(notice.message, "Der Begleitserver startete nicht", "the failure is translated");
    assert_eq!(notice.title, "Alert");
    assert!(app.ui.dialog.is_none());
    assert!(
        !backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "voice"),
        "the failure is not read aloud"
    );
    drop(app.take_threads());
}

#[test]
fn the_startup_page_opens_at_start_wind_included() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["start_page"] = json!("Motion");
    let app = loaded(backend.clone());
    assert_eq!(app.ui.screen, Screen::Motion);
    // Wind, the first choice and the default, opens like any other choice.
    let backend = FakeBackend::new();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["app"]["start_page"], json!("Wind"));
    let mut app = loaded(backend.clone());
    assert_eq!(app.ui.screen, Screen::Wind, "the default opens Wind");
    navigate(&mut app, Screen::Home, 0);
    open_startup_page(&mut app);
    assert_eq!(app.ui.screen, Screen::Wind, "a chosen Wind opens Wind");
    // A stored name that is none of the choices opens the first choice.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["start_page"] = json!("Nowhere");
    let app = loaded(backend.clone());
    assert_eq!(app.ui.screen, Screen::Wind);
    // HapticR while it is off is not offered: the start screen stays.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["start_page"] = json!("HapticR");
    let mut app = loaded(backend.clone());
    assert_eq!(app.ui.screen, Screen::Home);
    app.ui.screen = Screen::Home;
    backend.docs.lock().unwrap()["settings"]["app"]["start_page"] = json!("Tach");
    app.refresh_docs();
    open_startup_page(&mut app);
    assert_eq!(app.ui.screen, Screen::Tach);
    drop(app.take_threads());
}

#[test]
fn every_group_renders_in_every_theme() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    for theme in Theme::ALL {
        app.ui.theme = theme;
        for index in 0..page().sections.len() {
            app.ui.section = index;
            let html = main(&app);
            assert!(html.starts_with("<section class=\"panel\">"), "{theme:?} {index}");
        }
    }
    drop(app.take_threads());
}

fn max_speed(backend: &FakeBackend) -> serde_json::Value {
    backend.docs.lock().unwrap()["tune"]["default_max_speed_kmh"].clone()
}

#[test]
fn a_unit_change_reads_the_max_speed_from_the_service() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    // The service's profile changed; the screen's documents were not read again.
    backend.docs.lock().unwrap()["tune"]["default_max_speed_kmh"] = json!(400);
    dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap();
    assert_eq!(max_speed(&backend), json!(399), "400 km/h shows as 248 mph, stored again as 399 km/h");
    backend.docs.lock().unwrap()["tune"]["default_max_speed_kmh"] = json!(321);
    dispatch(&mut app, "set", "App:measurement_unit", "0").unwrap();
    assert_eq!(backend.patches("profile").last(), Some(&json!({"default_max_speed_kmh": 321})));
    drop(app.take_threads());
}

#[test]
fn a_unit_change_stores_the_unit_before_the_max_speed_is_read() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    backend.docs.lock().unwrap()["tune"].as_object_mut().unwrap().remove("default_max_speed_kmh");
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap_err();
    assert!(error.contains("default_max_speed_kmh"), "{error}");
    assert_eq!(stored(&backend, "speed_unit"), json!("mph"));
    assert_eq!(app.setting(&["app", "speed_unit"]), &json!("mph"));
    let calls = backend.calls.lock().unwrap();
    let stored_at = calls.iter().position(|(m, p, _)| *m == Method::Patch && p == "settings").unwrap();
    let read_at = calls.iter().position(|(m, p, _)| *m == Method::Get && p == "profile").unwrap();
    assert!(stored_at < read_at, "the unit is stored first");
    assert!(!calls.iter().any(|(m, p, _)| *m == Method::Patch && p == "profile"));
    drop(calls);
    drop(app.take_threads());
}

#[test]
fn a_max_speed_that_is_no_number_is_refused_after_the_unit_is_stored() {
    for value in [json!("120"), serde_json::Value::Null, json!([]), json!({})] {
        let backend = FakeBackend::new();
        let mut app = on_app(&backend);
        app.set_tune(&["default_max_speed_kmh"], value.clone());
        let writes = backend.patches("profile").len();
        let error = dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap_err();
        assert!(error.contains("TypeError:"), "{value}: {error}");
        assert_eq!(stored(&backend, "speed_unit"), json!("mph"));
        assert_eq!(app.tune(&["default_max_speed_kmh"]), &value);
        assert_eq!(backend.patches("profile").len(), writes, "{value}");
        drop(app.take_threads());
    }
    // The same unit chosen again: refused all the same, and nothing is written.
    for value in [json!("bad"), serde_json::Value::Null, json!([]), json!({})] {
        let backend = FakeBackend::new();
        let mut app = on_app(&backend);
        app.set_tune(&["default_max_speed_kmh"], value.clone());
        let writes = backend.patches("profile").len();
        let error = dispatch(&mut app, "set", "App:measurement_unit", "0").unwrap_err();
        assert!(error.contains("TypeError:"), "{value}: {error}");
        assert_eq!(backend.patches("profile").len(), writes, "{value}");
        drop(app.take_threads());
    }
    // In km/h, where the speed is shown as stored, a number written as text is accepted.
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    dispatch(&mut app, "set", "App:measurement_unit", "1").unwrap();
    app.set_tune(&["default_max_speed_kmh"], json!("120"));
    dispatch(&mut app, "set", "App:measurement_unit", "0").unwrap();
    assert_eq!(stored(&backend, "speed_unit"), json!("kmh"));
    drop(app.take_threads());
}

#[test]
fn a_choice_outside_the_units_or_levels_stores_nothing() {
    let backend = FakeBackend::new();
    let mut app = on_app(&backend);
    let (unit, level) = (stored(&backend, "speed_unit"), stored(&backend, "log_level"));
    backend.calls.lock().unwrap().clear();
    for value in ["7", "x"] {
        assert!(dispatch(&mut app, "set", "App:measurement_unit", value).is_err(), "{value}");
        assert!(dispatch(&mut app, "set", "App:log_level", value).is_err(), "{value}");
    }
    assert_eq!((stored(&backend, "speed_unit"), stored(&backend, "log_level")), (unit, level));
    assert!(backend.patches("settings").is_empty());
    drop(app.take_threads());
}

#[test]
fn an_unknown_unit_shows_as_mph_and_a_stored_level_by_its_name() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["app"]["speed_unit"] = json!("unknown");
        docs["settings"]["app"]["log_level"] = json!(40);
    }
    let mut app = on_app(&backend);
    dispatch(&mut app, "section", "", &group("Measurement")).unwrap();
    assert!(main(&app).contains("selected>mph</option>"));
    dispatch(&mut app, "section", "", &group("Logging")).unwrap();
    assert!(main(&app).contains("selected>Error</option>"));
    drop(app.take_threads());
}

/// Settings → Help holds the help and the version; the Motion screen's Tuning Help and a sound notice's
/// Learn More open it; none of them writes to the service or opens an external website.
#[test]
fn help_entry_points_open_the_help_screen_without_service_writes() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["notifications"] = json!([
        {"id":7,"kind":"surround_required","title":"Surround required","message":"Configure surround","colour":"red"}
    ]);
    let mut app = loaded(backend.clone());
    let before = backend.calls.lock().unwrap().len();
    app.gui.take_javascript();
    dispatch(&mut app, "nav", "Settings", "").unwrap();
    dispatch(&mut app, "screen", "", "Help").unwrap();
    assert_eq!(app.ui.screen, Screen::Help);
    let html = crate::ui::screens::main(&app);
    assert!(html.contains(&format!("OpenRigCue {}", env!("CARGO_PKG_VERSION"))));
    assert!(html.contains("log.txt") && html.contains("Games → Telemetry"));
    assert!(html.contains("Use SRS Sound Samples"), "imported recordings remain correctly attributed");
    assert!(html.contains("data-act=\"companion\""), "the mobile companion is offered here");
    // Tuning Help is a row button of the Motion screen, which takes clicks while a game runs.
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("acc");
    app.ui.session = crate::ui::session::read(&*app.backend);
    navigate(&mut app, Screen::Motion, 0);
    dispatch(&mut app, "action", "Motion:intellitune:0", "").unwrap();
    assert_eq!(app.ui.screen, Screen::Help, "Tuning Help");
    navigate(&mut app, Screen::Home, 0);
    let notice = app.ui.notices.service[&7];
    dispatch(&mut app, "notice", &format!("{notice}:0"), "").unwrap();
    assert_eq!(app.ui.screen, Screen::Help, "Learn More");
    let scripts = app.gui.take_javascript();
    assert!(scripts.iter().all(|script| script.starts_with("window.scrollTo")), "{scripts:?}");
    let calls = backend.calls.lock().unwrap();
    // Each navigation writes the settings file (`i18n/reapply`); nothing else is written.
    assert!(
        calls[before..].iter().all(|(method, path, _)| *method == Method::Get || path == "i18n/reapply"),
        "help must leave service state alone: {:?}",
        &calls[before..]
    );
    drop(calls);
    drop(app.take_threads());
}

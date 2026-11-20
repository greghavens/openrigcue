//! Tests of Tuning → Tach: the brightness, the output test and the ways to Hardware and LED.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate};

fn tach(backend: &Arc<FakeBackend>) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("acc");
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Tach, 0);
    app
}

#[test]
fn the_brightness_is_part_of_the_profile_and_the_buttons_lead_on() {
    let backend = FakeBackend::new();
    let mut app = tach(&backend);
    let html = main(&app);
    assert!(html.contains("data-arg=\"Tach:shift_light_dim_perc\"") && html.contains(">Configure tachometer<"));
    assert!(html.contains(">Configure LEDs<") && html.contains(">Test<"));
    dispatch(&mut app, "set", "Tach:shift_light_dim_perc", "45").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["gear_shift_dim_perc"], json!(45));
    dispatch(&mut app, "action", "Tach:hardware:", "").unwrap();
    assert_eq!((app.ui.screen, app.ui.section), (Screen::Hardware, 0));
    navigate(&mut app, Screen::Tach, 0);
    dispatch(&mut app, "action", "Tach:led:", "").unwrap();
    assert_eq!(app.ui.screen, Screen::Led);
    drop(app.take_threads());
}

#[test]
fn the_test_keeps_the_tachometer_on_then_restores_its_mode() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["tach"] = json!(1);
    let mut app = tach(&backend);
    drop(app.take_threads());
    let patches_before = backend.patches("controls").len();
    dispatch(&mut app, "action", "Tach:test:", "").unwrap();
    assert!(main(&app).contains("Testing…"));
    dispatch(&mut app, "action", "Tach:test:", "").unwrap();
    assert_eq!(app.take_threads().len(), 1, "a second test waits for the first");
    super::super::hardware::finish_test(&mut app).unwrap();
    let patches: Vec<_> = backend.patches("controls").into_iter().skip(patches_before).collect();
    assert_eq!(patches, [json!({"switches": {"tach": "always_on"}}), json!({"switches": {"tach": "auto"}})]);
    assert!(
        main(&app).contains(">Test<")
            && !main(&app).contains("id=\"c-tach-test\" data-act=\"action\" data-arg=\"Tach:test:\" disabled")
    );
}

#[test]
fn one_device_test_runs_at_a_time_here_and_on_the_hardware_screen() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["tach"] = json!(1);
    let mut app = tach(&backend);
    drop(app.take_threads());
    // A Hardware test runs: the Tach test is not offered and does not start.
    super::super::hardware::start_test(&mut app, &Test::Wind).unwrap();
    let html = main(&app);
    assert!(html.contains("data-arg=\"Tach:test:\" disabled>Test<"), "{html}");
    let before = backend.patches("controls").len();
    dispatch(&mut app, "action", "Tach:test:", "").unwrap();
    assert_eq!(backend.patches("controls").len(), before, "the tachometer stays as it is");
    assert_eq!(app.ui.devices.hardware.test.as_ref().map(|test| test.target.clone()), Some(Test::Wind));
    super::super::hardware::finish_test(&mut app).unwrap();
    // The Tach test runs: the Hardware screen's tests wait for it.
    dispatch(&mut app, "action", "Tach:test:", "").unwrap();
    super::super::hardware::start_test(&mut app, &Test::Led).unwrap();
    assert_eq!(app.ui.devices.hardware.test.as_ref().map(|test| test.target.clone()), Some(Test::Tach));
    super::super::hardware::finish_test(&mut app).unwrap();
    drop(app.take_threads());
}

#[test]
fn the_brightness_is_not_locked_in_stand_by() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Tach, 0);
    assert_eq!(app.ui.session.game_key, "none", "stand-by");
    let html = main(&app);
    assert!(!html.contains("Stand By mode"), "no stand-by note");
    let input = html.split("data-arg=\"Tach:shift_light_dim_perc\"").nth(1).unwrap().split('>').next().unwrap();
    assert!(!input.contains("disabled"), "{input}");
    dispatch(&mut app, "set", "Tach:shift_light_dim_perc", "30").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["tune"]["gear_shift_dim_perc"], json!(30));
    drop(app.take_threads());
}

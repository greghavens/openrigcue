//! Tests of the start screen's tuning panel: each tab draws and takes its rows with the rules, guards and
//! disabled state of its full screen (the stand-by lock, the running game's feature rule, the per-control
//! switch, the fan count).

use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Screen};

use super::super::tuning::game_without;

/// The app on the start screen at `tab`, with `game` running.
fn home(backend: &Arc<FakeBackend>, game: &str, tab: usize) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Home, 0);
    dispatch(&mut app, "tab", &tab.to_string(), "").unwrap();
    app
}

fn stored(backend: &FakeBackend, path: &[&str]) -> Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["tune"].clone(), |value, key| value[*key].clone())
}

fn feature_flag(app: &mut MyApp, backend: &FakeBackend, flag: Value) {
    backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = flag;
    app.refresh_docs();
}

/// The markup of a row of the panel, when it is shown.
fn row(app: &MyApp, control: &str) -> Option<String> {
    let html = main(app);
    let start = html.find(&format!("data-control=\"{control}\""))?;
    let rest = &html[start..];
    let end = rest[1..].find("data-control=\"").map_or(rest.len(), |i| i + 1);
    Some(rest[..end].to_owned())
}

#[test]
fn the_wind_tab_disables_the_curving_effect_with_one_fan() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"] = json!("1");
    let mut app = home(&backend, "acc", 2);
    let curving = row(&app, "curving_effect").expect("the row is shown");
    assert!(curving.contains("Needs 2 fans (Hardware)") && curving.contains(" disabled"), "{curving}");
    let before = stored(&backend, &["curving_effect"]);
    dispatch(&mut app, "set", "Tuning:curving_effect", "5").unwrap();
    assert_eq!(stored(&backend, &["curving_effect"]), before, "one fan: nothing is written");
    backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"] = json!("2");
    app.refresh_docs();
    assert!(!row(&app, "curving_effect").unwrap().contains(" disabled"));
    dispatch(&mut app, "set", "Tuning:curving_effect", "5").unwrap();
    assert_eq!(stored(&backend, &["curving_effect"]), json!(5));
    // The power sliders are the ends of the Wind screen's power range.
    dispatch(&mut app, "set", "Tuning:wind_max", "80").unwrap();
    assert_eq!(stored(&backend, &["power_wind_max_speed_perc"]), json!(80));
    dispatch(&mut app, "set", "Tuning:wind_min", "90").unwrap();
    assert_eq!(stored(&backend, &["power_wind_min_speed_perc"]), json!(80), "the low end stays below the high end");
    drop(app.take_threads());
}

#[test]
fn the_wind_tab_is_locked_in_stand_by_unless_a_fan_is_always_on() {
    let backend = FakeBackend::new();
    let mut app = home(&backend, "none", 2);
    let before = stored(&backend, &["power_wind_min_speed_perc"]);
    dispatch(&mut app, "set", "Tuning:wind_min", "5").unwrap();
    assert_eq!(stored(&backend, &["power_wind_min_speed_perc"]), before, "nothing is written while locked");
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["wind"] = json!(2);
    app.refresh_docs();
    dispatch(&mut app, "set", "Tuning:wind_min", "5").unwrap();
    assert_eq!(stored(&backend, &["power_wind_min_speed_perc"]), json!(5));
    drop(app.take_threads());
}

#[test]
fn the_shake_tab_follows_the_stand_by_lock_the_feature_rule_and_the_switch() {
    // Stand-by: shown disabled, and nothing is written.
    let backend = FakeBackend::new();
    let mut app = home(&backend, "none", 1);
    assert!(row(&app, "engine").unwrap().contains(" disabled"));
    let before = stored(&backend, &["shaker_effects", "engine"]);
    dispatch(&mut app, "set", "Tuning:engine", "13").unwrap();
    assert_eq!(stored(&backend, &["shaker_effects", "engine"]), before, "stand-by: nothing is written");
    drop(app.take_threads());
    // A game without the road bump's column (heave).
    let backend = FakeBackend::new();
    let mut app = home(&backend, &game_without("heave"), 1);
    let before = stored(&backend, &["shaker_effects", "road_bump"]);
    feature_flag(&mut app, &backend, json!(false));
    let road = row(&app, "road_bump").expect("shown while the rule disables");
    assert!(road.contains("Not used by this game") && road.contains(" disabled"), "{road}");
    dispatch(&mut app, "set", "Tuning:road_bump", "9").unwrap();
    assert_eq!(stored(&backend, &["shaker_effects", "road_bump"]), before);
    feature_flag(&mut app, &backend, json!(true));
    assert!(row(&app, "road_bump").is_none(), "hidden while the rule hides");
    dispatch(&mut app, "set", "Tuning:road_bump", "9").unwrap();
    assert_eq!(stored(&backend, &["shaker_effects", "road_bump"]), before);
    // A value that is no yes or no: the rule does not apply.
    feature_flag(&mut app, &backend, json!("on"));
    assert!(!row(&app, "road_bump").unwrap().contains(" disabled"));
    dispatch(&mut app, "set", "Tuning:road_bump", "9").unwrap();
    assert_eq!(stored(&backend, &["shaker_effects", "road_bump"]), json!(9));
    // The effect's switch off disables it here too.
    backend.docs.lock().unwrap()["tune"]["shaker_switch"]["engine"] = json!(false);
    app.refresh_docs();
    assert!(row(&app, "engine").unwrap().contains(" disabled"));
    drop(app.take_threads());
}

#[test]
fn the_motion_tab_follows_the_feature_rule_and_the_switch() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["motion_switch"]["roll"] = json!(true);
    let mut app = home(&backend, &game_without("roll"), 0);
    let before = stored(&backend, &["motion_effects", "roll"]);
    feature_flag(&mut app, &backend, json!(false));
    let roll = row(&app, "roll").expect("shown while the rule disables");
    assert!(roll.contains(" disabled"), "{roll}");
    dispatch(&mut app, "set", "Tuning:roll", "7").unwrap();
    assert_eq!(stored(&backend, &["motion_effects", "roll"]), before);
    feature_flag(&mut app, &backend, json!(true));
    assert!(row(&app, "roll").is_none(), "hidden while the rule hides");
    feature_flag(&mut app, &backend, json!("on"));
    assert!(!row(&app, "roll").unwrap().contains(" disabled"), "the rule does not apply");
    dispatch(&mut app, "set", "Tuning:roll", "7").unwrap();
    assert_eq!(stored(&backend, &["motion_effects", "roll"]), json!(7));
    backend.docs.lock().unwrap()["tune"]["motion_switch"]["roll"] = json!(false);
    app.refresh_docs();
    assert!(row(&app, "roll").unwrap().contains(" disabled"), "the switch off disables it");
    drop(app.take_threads());
}

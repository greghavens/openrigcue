//! Tests of the profile-load write-back: each row stores its value the way its control holds it, in order, a
//! required value that cannot be used stops the rows after it, HapticR's tables are emptied by an unusable
//! value, the STR-SCN6150 platforms take their fixed values, the axis types and the driving style follow the
//! profile, and the LED segments at start.

use std::sync::Arc;

use serde_json::{json, Value};

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::host::UiApp;
use crate::ui::screens::motion::PREFERENCE_PICKER;
use crate::ui::{dispatch, navigate, Screen};

fn app_for(backend: &Arc<FakeBackend>, game: &str) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    app
}

fn tune(backend: &FakeBackend, path: &[&str]) -> Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["tune"].clone(), |value, key| value[*key].clone())
}

/// Sets tune values, then loads the profile again.
fn load(app: &mut MyApp, backend: &FakeBackend, values: &[(&[&str], Value)]) -> Result<(), String> {
    {
        let mut docs = backend.docs.lock().unwrap();
        for (path, value) in values {
            crate::backend::set_at(&mut docs["tune"], path, value.clone());
        }
    }
    app.refresh_docs();
    let result = profile_load(app);
    drop(app.take_threads());
    result
}

#[test]
fn sliders_round_to_their_step_and_keep_within_their_range() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    load(
        &mut app,
        &backend,
        &[
            (&["motion_effects", "pitch"], json!(7.6)),
            (&["motion_effects", "heave"], json!(25)),
            (&["motion_max_power"], json!(-3)),
            (&["motion_boost_factor"], json!(0)),
            (&["motion_effect_smoothing_factor", "sway"], json!(42.5)),
            (&["motion_main_actuators_surge"], json!(130)),
            (&["shaker_effects", "engine"], json!(12.5)),
            (&["shaker_volume", "fl"], json!(21)),
            (&["rumble_effects", "throttle_engine"], json!(9.7)),
            (&["rumble_effects", "rpm_start_fading"], json!("70.6")),
            (&["rumble_effects", "rpm_stop_fading"], json!(60.9)),
            (&["rumble_effects", "wheel_slip_min_activation"], json!(33.8)),
            (&["belt_surge"], json!(0)),
            (&["belt_heave_smoothing"], json!(101)),
            (&["gear_shift_perc"], json!(150)),
            (&["gear_shift_dim_perc"], json!(-1)),
            (&["speed_factor"], json!(0)),
            (&["power_wind_min_speed_perc"], json!(70)),
            (&["power_wind_max_speed_perc"], json!(40)),
        ],
    )
    .unwrap();
    for (path, expected) in [
        (&["motion_effects", "pitch"][..], json!(8)),
        (&["motion_effects", "heave"], json!(20)),
        (&["motion_max_power"], json!(0)),
        (&["motion_boost_factor"], json!(1)),
        (&["motion_effect_smoothing_factor", "sway"], json!(42)),
        (&["motion_main_actuators_surge"], json!(100)),
        (&["shaker_effects", "engine"], json!(12)),
        (&["shaker_volume", "fl"], json!(20)),
        (&["rumble_effects", "throttle_engine"], json!(10)),
        // The fade start is read as a number and truncated, the stop truncated; then ordered.
        (&["rumble_effects", "rpm_start_fading"], json!(60)),
        (&["rumble_effects", "rpm_stop_fading"], json!(70)),
        (&["rumble_effects", "wheel_slip_min_activation"], json!(33)),
        (&["belt_surge"], json!(1)),
        (&["belt_heave_smoothing"], json!(100)),
        (&["gear_shift_perc"], json!(100)),
        (&["gear_shift_dim_perc"], json!(0)),
        (&["speed_factor"], json!(1)),
        (&["power_wind_min_speed_perc"], json!(40)),
        (&["power_wind_max_speed_perc"], json!(70)),
    ] {
        assert_eq!(tune(&backend, path), expected, "{path:?}");
    }
}

#[test]
fn values_already_as_their_control_holds_them_are_not_written_again() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    load(&mut app, &backend, &[]).unwrap();
    backend.calls.lock().unwrap().clear();
    load(&mut app, &backend, &[]).unwrap();
    assert_eq!(backend.patches("profile"), Vec::<Value>::new());
}

#[test]
fn the_smoothing_rows_follow_the_smoothing_step() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["features"]["smoothing_slider_step"] = json!(5);
    let mut app = app_for(&backend, "acc");
    load(&mut app, &backend, &[(&["motion_effect_smoothing_factor", "yaw"], json!(52))]).unwrap();
    assert_eq!(tune(&backend, &["motion_effect_smoothing_factor", "yaw"]), json!(50));
}

#[test]
fn spin_boxes_keep_within_their_range() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    load(
        &mut app,
        &backend,
        &[
            (&["thanos_spike_filter_vertical"], json!(70000)),
            (&["thanos_spike_filter_horizontal"], json!(10.9)),
            (&["min_traction_loss_activation"], json!(-4)),
            (&["min_traction_loss_front_activation"], json!(2500)),
            (&["max_telemetry_vars", "sway"], json!(-2000)),
            (&["max_telemetry_vars", "heave"], json!(99.7)),
        ],
    )
    .unwrap();
    for (path, expected) in [
        (&["thanos_spike_filter_vertical"][..], 64530),
        (&["thanos_spike_filter_horizontal"], 255),
        (&["min_traction_loss_activation"], 0),
        (&["min_traction_loss_front_activation"], 1800),
        (&["max_telemetry_vars", "sway"], -1800),
        (&["max_telemetry_vars", "heave"], 99),
    ] {
        assert_eq!(tune(&backend, path), json!(expected), "{path:?}");
    }
}

#[test]
fn the_traction_loss_thresholds_and_gear_smoothing_are_left_alone_outside_racing_games() {
    let backend = FakeBackend::new();
    let flight = openrigcue_config::catalog::Catalog::builtin()
        .games()
        .iter()
        .find(|game| game.kind != openrigcue_config::catalog::GameKind::Racing)
        .expect("a game that is no racing game")
        .id
        .clone();
    let mut app = app_for(&backend, &flight);
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(flight);
    load(
        &mut app,
        &backend,
        &[(&["min_traction_loss_activation"], json!(5000)), (&["gear_smoothing_factor"], json!(250))],
    )
    .unwrap();
    assert_eq!(tune(&backend, &["min_traction_loss_activation"]), json!(5000));
    assert_eq!(tune(&backend, &["gear_smoothing_factor"]), json!(250));
}

#[test]
fn the_wind_speed_loses_precision_through_miles_per_hour() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    backend.docs.lock().unwrap()["settings"]["app"]["speed_unit"] = json!("mph");
    // 100 km/h shows as 62 mph (truncated), which is stored as 99 km/h (truncated); 99 then becomes 98.
    load(&mut app, &backend, &[(&["default_max_speed_kmh"], json!(100))]).unwrap();
    assert_eq!(tune(&backend, &["default_max_speed_kmh"]), json!(99));
    load(&mut app, &backend, &[]).unwrap();
    assert_eq!(tune(&backend, &["default_max_speed_kmh"]), json!(98));
    backend.docs.lock().unwrap()["settings"]["app"]["speed_unit"] = json!("kmh");
    load(&mut app, &backend, &[(&["default_max_speed_kmh"], json!(2500))]).unwrap();
    assert_eq!(tune(&backend, &["default_max_speed_kmh"]), json!(2000));
}

#[test]
fn one_fan_switches_wind_curving_off_and_missing_wind_values_take_their_defaults() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"] = json!("1");
    {
        let mut docs = backend.docs.lock().unwrap();
        let tune = docs["tune"].as_object_mut().unwrap();
        tune.remove("enable_ejection_seat");
        tune.remove("power_center_wind_max_speed_perc");
    }
    load(&mut app, &backend, &[(&["wind_curving"], json!(true)), (&["curving_effect"], json!(6))]).unwrap();
    assert_eq!(tune(&backend, &["wind_curving"]), json!(0));
    assert_eq!(tune(&backend, &["curving_effect"]), json!(1));
    assert_eq!(tune(&backend, &["enable_ejection_seat"]), json!(0));
    assert_eq!(tune(&backend, &["power_center_wind_min_speed_perc"]), json!(0));
    assert_eq!(tune(&backend, &["power_center_wind_max_speed_perc"]), json!(100));
}

#[test]
fn a_required_value_that_cannot_be_used_stops_the_rows_after_it() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    // A Belt switch that is no boolean word: the Wind speed before it is written, the Wind ranges after it not.
    let error = load(
        &mut app,
        &backend,
        &[
            (&["speed_factor"], json!(30)),
            (&["belt_surge_enabled"], json!("on")),
            (&["power_wind_min_speed_perc"], json!(-5)),
        ],
    )
    .unwrap_err();
    assert!(error.contains("Boolean value expected"), "{error}");
    assert_eq!(tune(&backend, &["speed_factor"]), json!(10));
    assert_eq!(tune(&backend, &["power_wind_min_speed_perc"]), json!(-5));
    // The tuning rule logs the error and goes on.
    super::super::tuning::on_profile_load(&mut app);
    drop(app.take_threads());
}

#[test]
fn hapticr_empties_a_table_with_an_unusable_value() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = app_for(&backend, "acc");
    let effects: Vec<String> = Registry::builtin().haptic_effects.iter().map(|e| e.id.clone()).collect();
    let (first, second) = (effects[0].clone(), effects[1].clone());
    let mut values: Vec<(Vec<String>, Value)> = Vec::new();
    for effect in &effects {
        values.push((vec!["haptic_effects".into(), effect.clone()], json!(9)));
        values.push((vec!["haptic_switch".into(), effect.clone()], json!(false)));
    }
    // The second effect has no strength and its switch is no yes or no.
    values.push((vec!["haptic_effects".into(), second.clone()], Value::Null));
    values.push((vec!["haptic_switch".into(), second.clone()], json!("on")));
    values.push((vec!["haptic_effects".into(), "wheel_slip_min_activation".into()], json!(12)));
    let values: Vec<(Vec<&str>, Value)> =
        values.iter().map(|(path, value)| (path.iter().map(String::as_str).collect(), value.clone())).collect();
    let values: Vec<(&[&str], Value)> = values.iter().map(|(path, value)| (&path[..], value.clone())).collect();
    load(&mut app, &backend, &values).unwrap();
    // The second effect empties both tables, the first effect's values with them; every effect after it then
    // has no value and empties them again, so only the last keeps the 0 and the on it took.
    assert!(effects.len() > 2);
    let last = effects.last().unwrap().clone();
    for effect in &effects[..effects.len() - 1] {
        assert_eq!(tune(&backend, &["haptic_effects", effect]), Value::Null, "{effect}");
        assert_eq!(tune(&backend, &["haptic_switch", effect]), Value::Null, "{effect}");
    }
    assert_eq!(tune(&backend, &["haptic_effects", &last]), json!(0));
    assert_eq!(tune(&backend, &["haptic_switch", &last]), json!(true));
    // The advanced rows take their fallbacks.
    assert_eq!(tune(&backend, &["haptic_effects", "wheel_slip_min_activation"]), json!(30));
    assert_eq!(tune(&backend, &["haptic_effects", "rpm_start_fading"]), json!(94));
    assert_eq!(tune(&backend, &["haptic_effects", "rpm_stop_fading"]), json!(100));
    // A switch value other than a yes or no word counts as missing: the switch table alone is emptied.
    backend.docs.lock().unwrap()["tune"]["haptic_effects"] = json!({});
    backend.docs.lock().unwrap()["tune"]["haptic_switch"] = json!({});
    let mut values: Vec<(Vec<String>, Value)> = Vec::new();
    for effect in &effects {
        values.push((vec!["haptic_effects".into(), effect.clone()], json!(5)));
        values.push((vec!["haptic_switch".into(), effect.clone()], json!(false)));
    }
    values.push((vec!["haptic_switch".into(), first.clone()], json!(1.0)));
    let values: Vec<(Vec<&str>, Value)> =
        values.iter().map(|(path, value)| (path.iter().map(String::as_str).collect(), value.clone())).collect();
    let values: Vec<(&[&str], Value)> = values.iter().map(|(path, value)| (&path[..], value.clone())).collect();
    load(&mut app, &backend, &values).unwrap();
    for effect in &effects {
        assert_eq!(tune(&backend, &["haptic_effects", effect]), json!(5), "{effect}");
    }
    assert_eq!(tune(&backend, &["haptic_switch", &first]), Value::Null);
    assert_eq!(tune(&backend, &["haptic_switch", &last]), json!(true));
}

#[test]
fn the_str_scn6150_platforms_take_their_fixed_values() {
    for (model, hidden) in [("STR-SCN6150", &motion::STR_HIDDEN[..]), ("STR-SCN6150-TL", &motion::STR_TL_HIDDEN[..])] {
        let backend = FakeBackend::new();
        let mut app = app_for(&backend, "acc");
        // The platform is set after the start, which puts a model the model table lacks back to None.
        backend.docs.lock().unwrap()["settings"]["motion"]["model"] = json!(model);
        let mut values: Vec<(Vec<String>, Value)> = vec![
            (vec!["motion_max_horizontal".into()], json!(40)),
            (vec!["motion_smoothing_factor".into()], json!(70)),
            (vec!["min_traction_loss_activation".into()], json!(300)),
            (vec!["min_traction_loss_front_activation".into()], json!(300)),
            (vec!["motion_main_actuators_sway".into()], json!(10)),
            (vec!["motion_main_actuators_surge".into()], json!(10)),
            (vec!["motion_main_actuators_traction_loss".into()], json!(60)),
            (vec!["motion_main_actuators_traction_loss_front".into()], json!(60)),
        ];
        for effect in hidden {
            values.push((vec!["motion_switch".into(), (*effect).into()], json!(true)));
        }
        let values: Vec<(Vec<&str>, Value)> =
            values.iter().map(|(path, value)| (path.iter().map(String::as_str).collect(), value.clone())).collect();
        let values: Vec<(&[&str], Value)> = values.iter().map(|(path, value)| (&path[..], value.clone())).collect();
        // The model table does not list these platforms: the Motion rows stop at the model lookup, before the
        // platform's values, as the load always has.
        let error = load(&mut app, &backend, &values).unwrap_err();
        assert!(error.starts_with("KeyError") && error.contains(model), "{error}");
        assert_eq!(tune(&backend, &["motion_max_horizontal"]), json!(40));
        // The platform's values themselves.
        str_values(&mut app).unwrap();
        for (key, expected) in [
            ("motion_max_horizontal", 100),
            ("motion_smoothing_factor", 1),
            ("min_traction_loss_activation", 0),
            ("min_traction_loss_front_activation", 0),
            ("motion_main_actuators_sway", 100),
            ("motion_main_actuators_surge", 100),
            ("motion_main_actuators_traction_loss", 0),
            ("motion_main_actuators_traction_loss_front", 0),
        ] {
            assert_eq!(tune(&backend, &[key]), json!(expected), "{model} {key}");
        }
        for effect in hidden {
            let expected = if *effect == "traction_loss" { json!(true) } else { json!(false) };
            assert_eq!(tune(&backend, &["motion_switch", effect]), expected, "{model} {effect}");
        }
    }
    // Other platforms keep the profile's values.
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    load(&mut app, &backend, &[(&["motion_max_horizontal"], json!(40))]).unwrap();
    assert_eq!(tune(&backend, &["motion_max_horizontal"]), json!(40));
}

#[test]
fn the_axis_types_are_angle_or_rate_and_need_their_maximum() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    load(
        &mut app,
        &backend,
        &[
            (&["motion_effects", "roll_type"], json!("degrees")),
            (&["max_telemetry_vars", "roll_rate"], json!(5000)),
            (&["motion_effects", "pitch_type"], json!("angle")),
        ],
    )
    .unwrap();
    assert_eq!(tune(&backend, &["motion_effects", "roll_type"]), json!("rate"));
    assert_eq!(tune(&backend, &["max_telemetry_vars", "roll_rate"]), json!(1800));
    assert!(!app.ui.tuning.motion_hidden.contains("roll_type"));
    // Without the maximum of its type, or without a type, the row is hidden.
    backend.docs.lock().unwrap()["tune"]["max_telemetry_vars"].as_object_mut().unwrap().remove("roll_rate");
    backend.docs.lock().unwrap()["tune"]["motion_effects"].as_object_mut().unwrap().remove("pitch_type");
    load(&mut app, &backend, &[]).unwrap();
    assert!(app.ui.tuning.motion_hidden.contains("roll_type") && app.ui.tuning.motion_hidden.contains("pitch_type"));
    navigate(&mut app, Screen::Motion, 0);
    let advanced =
        crate::spec::motion_page(Registry::builtin(), 1.0).sections.iter().position(|s| s.title == "Advanced").unwrap();
    dispatch(&mut app, "section", "", &advanced.to_string()).unwrap();
    let html = motion::main(&app);
    assert!(!html.contains("data-control=\"roll_type\"") && !html.contains("data-control=\"pitch_type\""));
    // The maximum back: shown again.
    load(&mut app, &backend, &[(&["max_telemetry_vars", "roll_rate"], json!(20))]).unwrap();
    assert!(motion::main(&app).contains("data-control=\"roll_type\""));
    drop(app.take_threads());
}

/// An axis without a type follows the angle: its angle maximum is kept within its box, the rate maximum is left.
#[test]
fn an_axis_without_a_type_keeps_its_angle_maximum_within_the_box() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    backend.docs.lock().unwrap()["tune"]["motion_effects"].as_object_mut().unwrap().remove("roll_type");
    load(
        &mut app,
        &backend,
        &[(&["max_telemetry_vars", "roll"], json!(5000)), (&["max_telemetry_vars", "roll_rate"], json!(5000))],
    )
    .unwrap();
    assert_eq!(tune(&backend, &["max_telemetry_vars", "roll"]), json!(1800));
    assert_eq!(tune(&backend, &["max_telemetry_vars", "roll_rate"]), json!(5000));
    assert!(app.ui.tuning.motion_hidden.contains("roll_type"));
    assert_eq!(tune(&backend, &["motion_effects", "roll_type"]), Value::Null, "no type is stored");
    drop(app.take_threads());
}

#[test]
fn the_driving_style_is_offered_with_a_stored_style_only() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    navigate(&mut app, Screen::Motion, 0);
    dispatch(&mut app, "section", "", "1").unwrap();
    for (style, offered) in [(json!("Medium"), true), (json!(""), false), (Value::Null, true)] {
        load(&mut app, &backend, &[(&["intellitune_motion_preference"], style.clone())]).unwrap();
        assert_eq!(!app.ui.tuning.motion_hidden.contains(PREFERENCE_PICKER), offered, "{style}");
        let html = motion::main(&app);
        assert_eq!(html.contains("id=\"c-motion-intellitune\" class=\"field-input\""), offered, "{style}");
        assert!(html.contains(">Tuning Help<"), "the row's buttons stay");
    }
    backend.docs.lock().unwrap()["tune"].as_object_mut().unwrap().remove("intellitune_motion_preference");
    load(&mut app, &backend, &[]).unwrap();
    assert!(app.ui.tuning.motion_hidden.contains(PREFERENCE_PICKER), "a profile without a style offers none");
    // Not offered: a choice sent anyway is not taken.
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Motion:intellitune", "2").unwrap();
    assert!(backend.patches("profile").is_empty());
    drop(app.take_threads());
}

#[test]
fn the_led_segments_keep_within_the_strip_and_take_a_listed_type_at_start() {
    let backend = FakeBackend::new();
    let mut app = app_for(&backend, "acc");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["leds"]["nbr_leds"] = json!(60);
        let segments = docs["settings"]["leds"]["segments"].as_array_mut().unwrap();
        segments[0]["start"] = json!(50);
        segments[0]["end"] = json!(90);
        segments[1]["start"] = json!(70);
        segments[1]["end"] = json!(20);
    }
    app.refresh_docs();
    // The type is stored as the code of a listed type; an unlisted one is Off.
    app.backend.assign_setting_value("LED", "segment3_type", &LegacyValue::Str("Z".into())).unwrap();
    app.backend.assign_setting_value("LED", "segment4_type", &LegacyValue::Str("SRF".into())).unwrap();
    start(&mut app).unwrap();
    let setting = |key: &str| app.backend.native_setting("LED", key).unwrap();
    assert_eq!((setting("segment1_start"), setting("segment1_end")), (LegacyValue::Int(50), LegacyValue::Int(60)));
    assert_eq!((setting("segment2_start"), setting("segment2_end")), (LegacyValue::Int(20), LegacyValue::Int(60)));
    assert_eq!(setting("segment3_type"), LegacyValue::Str("O".into()));
    assert_eq!(setting("segment4_type"), LegacyValue::Str("SRF".into()));
    drop(app.take_threads());
}

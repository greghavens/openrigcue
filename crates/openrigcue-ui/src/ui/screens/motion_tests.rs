//! Tests of Tuning → Motion: its rows and rules, every value it stores, its buttons, Autotune and the hot keys.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::Method;
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Region};

/// The app on the Motion screen with a game running (the tune is editable), loaded with a profile that has a
/// driving style (an empty one becomes Medium, so the choice is offered).
fn motion(backend: &std::sync::Arc<FakeBackend>) -> MyApp {
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("acc");
        if docs["tune"]["intellitune_motion_preference"] == json!("") {
            docs["tune"]["intellitune_motion_preference"] = json!("Medium");
        }
    }
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Motion, 0);
    drop(app.take_threads());
    app
}

fn body(app: &MyApp) -> String {
    main(app)
}

fn stored(backend: &FakeBackend, path: &[&str]) -> serde_json::Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["tune"].clone(), |value, key| value[*key].clone())
}

#[test]
fn every_group_of_the_page_is_offered_and_rows_follow_the_design_pattern() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let platform = body(&app);
    assert!(platform.contains("id=\"section-picker\""));
    for group in ["Platform", "Effects", "Racing", "Flight", "Smoothing Effects", "Advanced"] {
        assert!(platform.contains(&format!(">{group}</option>")), "{group}");
    }
    assert!(platform.contains("data-arg=\"Motion:motion_max_power\""));
    // The page's header buttons are under Actions; Center only for a connected Yaw VR.
    assert!(
        platform.contains(">Actions<") && platform.contains(">Hardware setup<") && platform.contains(">Shortcuts<")
    );
    assert!(!platform.contains("Move platform to center</button>"));
    // Effects: four rows, the rest under More settings; each effect's switch and Max Telemetry under Options.
    dispatch(&mut app, "section", "", "1").unwrap();
    let effects = body(&app);
    assert!(effects.contains("More settings · "));
    assert!(effects.contains("data-arg=\"Motion:pitch:toggle\"") && effects.contains("data-arg=\"Motion:pitch:max\""));
    assert!(effects.contains(">Options<"));
    assert!(
        effects.contains(">Autotune<") && effects.contains(">Tuning Help<") && effects.contains(">Go to Telemetry<")
    );
    drop(app.take_threads());
}

#[test]
fn every_motion_value_is_stored_as_the_page_stores_it() {
    let backend = FakeBackend::new();
    with_axis_types(&backend);
    let mut app = motion(&backend);
    for (arg, value, path, expected) in [
        ("Motion:motion_max_power", "55", vec!["motion_max_power"], json!(55)),
        ("Motion:motion_smoothing_factor", "35", vec!["motion_smoothing_factor"], json!(35)),
        ("Motion:motion_boost_factor", "7", vec!["motion_boost_factor"], json!(7)),
        ("Motion:pitch", "14", vec!["motion_effects", "pitch"], json!(14)),
        ("Motion:pitch:toggle", "0", vec!["motion_switch", "pitch"], json!(false)),
        ("Motion:pitch:toggle", "1", vec!["motion_switch", "pitch"], json!(true)),
        ("Motion:pitch:max", "40", vec!["max_telemetry_vars", "pitch"], json!(40)),
        ("Motion:motion_smoothing_heave", "20", vec!["motion_effect_smoothing_factor", "heave"], json!(20)),
        ("Motion:min_traction_loss_activation", "30", vec!["min_traction_loss_activation"], json!(30)),
        ("Motion:gear_smoothing_factor", "12", vec!["gear_smoothing_factor"], json!(12)),
        ("Motion:belt_tensioner", "60", vec!["motion_belt_tensioner"], json!(60)),
        ("Motion:enhanced_gforces", "1", vec!["motion_enhanced_gforces"], json!(1)),
        ("Motion:motion_main_actuators_sway", "80", vec!["motion_main_actuators_sway"], json!(80)),
        ("Motion:thanos_spike_filter_vertical", "300", vec!["thanos_spike_filter_vertical"], json!(300)),
    ] {
        // The rows' writes, whatever the platform offers (the rows the model hides take nothing from the
        // screen; see `hidden_and_disabled_rows_take_no_change`).
        let mut parts = arg.splitn(3, ':').skip(1);
        let (control, part) = (parts.next().unwrap(), parts.next().unwrap_or(""));
        write(&mut app, control, part, value).unwrap();
        assert_eq!(stored(&backend, &path), expected, "{arg}");
    }
    // Rate: the axis follows the rate and its Max Telemetry stores the rate maximum.
    backend.docs.lock().unwrap()["tune"]["max_telemetry_vars"]["pitch_rate"] = json!(30);
    app.refresh_docs();
    dispatch(&mut app, "set", "Motion:pitch_type", "1").unwrap();
    assert_eq!(stored(&backend, &["motion_effects", "pitch_type"]), json!("rate"));
    dispatch(&mut app, "set", "Motion:pitch:max", "25").unwrap();
    assert_eq!(stored(&backend, &["max_telemetry_vars", "pitch_rate"]), json!(25));
    // Reaction Speed also sets the platform's reaction speed; the refresh rate is a platform setting.
    backend.calls.lock().unwrap().clear();
    write(&mut app, "reaction_speed", "", "90").unwrap();
    assert_eq!(stored(&backend, &["motion_reaction_speed"]), json!(90));
    write(&mut app, "refresh_rate", "", "200").unwrap();
    let settings: Vec<_> = backend.patches("settings");
    assert!(settings.iter().any(|p| p["motion"]["reaction_speed"] == json!(90)), "{settings:?}");
    assert!(settings.iter().any(|p| p["motion"]["refresh_rate"] == json!(200)), "{settings:?}");
    drop(app.take_threads());
}

#[test]
fn a_driving_style_sets_the_effects_within_its_range() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    dispatch(&mut app, "set", "Motion:intellitune", "2").unwrap();
    assert_eq!(stored(&backend, &["intellitune_motion_preference"]), json!("Aggressive"));
    for effect in ["sway", "roll", "pitch", "heave"] {
        let value = stored(&backend, &["motion_effects", effect]).as_i64().unwrap();
        assert!((17..=19).contains(&value), "{effect} {value}");
    }
    assert_eq!(stored(&backend, &["motion_effects", "yaw"]), json!(0));
    assert_eq!(stored(&backend, &["motion_effect_smoothing_factor", "roll"]), json!(10));
    drop(app.take_threads());
}

#[test]
fn stand_by_locks_the_rows_and_offers_to_hide_the_notice() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Motion, 0);
    let html = body(&app);
    assert!(html.contains("Tuning is unavailable in Stand By mode"));
    assert!(html.contains("data-arg=\"Motion:motion_max_power\"") && html.contains(" disabled"));
    dispatch(&mut app, "standby-hide", "", "").unwrap();
    assert!(backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|(m, p, _)| *m == Method::Post && p == "settings/hide-standby-alert"));
    drop(app.take_threads());
}

#[test]
fn the_rows_follow_the_motion_model() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] =
        json!("6 Actuators (2 Front + 2 Rear + TL + Surge) CANBus");
    let mut app = motion(&backend);
    let platform = body(&app);
    assert!(!platform.contains("Motion:motion_smoothing_factor\""), "CANBus models hide Overall Smoothing");
    if platform.contains("Motion:reaction_speed\"") {
        assert!(platform.contains("Action Mode") && platform.contains("Smooth") && platform.contains("Vivid"));
    }
    dispatch(&mut app, "section", "", "6").unwrap();
    let actuators = body(&app);
    assert!(actuators.contains("Motion:motion_main_actuators_sway\""), "main actuators shown for this model");
    drop(app.take_threads());
}

#[test]
fn buttons_open_help_telemetry_hardware_and_shortcuts() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    dispatch(&mut app, "action", "Motion:intellitune:2", "").unwrap();
    assert_eq!(app.ui.screen, Screen::Telemetry);
    navigate(&mut app, Screen::Motion, 0);
    let page = spec::motion_page(Registry::builtin(), 1.0);
    let hardware =
        page.header.iter().position(|b| matches!(b, Widget::Button { text, .. } if text == "Hardware setup")).unwrap();
    dispatch(&mut app, "action", &format!("Motion:@header:{hardware}"), "").unwrap();
    assert_eq!(app.ui.screen, Screen::Hardware);
    navigate(&mut app, Screen::Motion, 0);
    let shortcuts =
        page.header.iter().position(|b| matches!(b, Widget::Button { text, .. } if text == "Shortcuts")).unwrap();
    dispatch(&mut app, "action", &format!("Motion:@header:{shortcuts}"), "").unwrap();
    let Some(Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("shortcut editor") };
    assert!(editor.actions.iter().any(|(key, _)| key == "motion_box_pitch_up"));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    // An effect's own shortcut button: the editor for that effect.
    // (a game that uses the effect, so its row takes clicks)
    let features = crate::catalog::features();
    let (effect, game) = Registry::builtin()
        .motion_effects
        .iter()
        .filter(|e| e.shortcut && !e.allow_max)
        .find_map(|e| {
            features
                .games
                .iter()
                .find(|(game, columns)| {
                    columns.contains(&e.id) && openrigcue_config::catalog::Catalog::builtin().game(game).is_some()
                })
                .map(|(game, _)| (e, game.clone()))
        })
        .unwrap();
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    app.refresh_docs();
    app.ui.session = crate::ui::session::read(&*app.backend);
    navigate(&mut app, Screen::Motion, 0);
    dispatch(&mut app, "action", &format!("Motion:{}:0", effect.id), "").unwrap();
    let Some(Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("effect shortcut editor") };
    assert_eq!(editor.actions[0].0, effect.id);
    drop(app.take_threads());
}

#[test]
fn autotune_runs_its_three_steps_and_cancel_stops_the_capture() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    dispatch(&mut app, "action", "Motion:intellitune:1", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(Dialog::Wizard(WizardStep::Intro))));
    assert!(crate::ui::render(&app, Region::Dialog).contains("Autotune - Step 1/3"));
    drop(app.take_threads());
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(Dialog::Wizard(WizardStep::Countdown(None)))));
    assert_eq!(app.take_threads().len(), 1, "the count-down runs on its own thread");
    assert!(crate::ui::render(&app, Region::Dialog).contains("data-act=\"dialog-ok\" disabled"));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    // The review applies each suggested maximum and the chosen preference.
    app.ui.dialog =
        Some(Dialog::Wizard(WizardStep::Review { rows: vec![("pitch".into(), "10".into(), 12)], preference: 0 }));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(stored(&backend, &["max_telemetry_vars", "pitch"]), json!(12));
    assert_eq!(stored(&backend, &["intellitune_motion_preference"]), json!("Soft"));
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

/// Autotune's Apply puts each suggestion into its Max Telemetry box (kept within −1800…1800) and offers the
/// driving style it stored at once, even when the profile had none.
#[test]
fn autotune_apply_keeps_the_maxima_within_the_boxes_and_offers_the_style() {
    let backend = FakeBackend::new();
    with_axis_types(&backend);
    let mut app = motion(&backend);
    app.ui.tuning.motion_hidden.insert(PREFERENCE_PICKER.to_owned());
    let rows = vec![("pitch".into(), "10".into(), 2500), ("roll".into(), "10".into(), 300)];
    app.ui.dialog = Some(Dialog::Wizard(WizardStep::Review { rows, preference: 2 }));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(stored(&backend, &["max_telemetry_vars", "pitch"]), json!(1800));
    assert_eq!(stored(&backend, &["max_telemetry_vars", "roll"]), json!(300));
    assert_eq!(stored(&backend, &["intellitune_motion_preference"]), json!("Aggressive"));
    assert!(!app.ui.tuning.motion_hidden.contains(PREFERENCE_PICKER), "the style is offered");
    let html = group(&mut app, EFFECTS);
    assert!(html.contains("id=\"c-motion-intellitune\""), "the picker shows at once");
    drop(app.take_threads());
}

/// Choosing Roll or Pitch's type puts the maximum of that type into its Max Telemetry box: it is stored as a
/// whole number within −1800…1800.
#[test]
fn an_axis_type_change_keeps_its_maximum_within_the_box() {
    let backend = FakeBackend::new();
    with_axis_types(&backend);
    let mut app = motion(&backend);
    for (maximum, expected) in [(json!(2500), json!(1800)), (json!(-4000), json!(-1800)), (json!(12.7), json!(12))] {
        backend.docs.lock().unwrap()["tune"]["max_telemetry_vars"]["pitch_rate"] = maximum.clone();
        backend.docs.lock().unwrap()["tune"]["motion_effects"]["pitch_type"] = json!("angle");
        app.refresh_docs();
        dispatch(&mut app, "set", "Motion:pitch_type", "1").unwrap();
        assert_eq!(stored(&backend, &["max_telemetry_vars", "pitch_rate"]), expected, "{maximum}");
    }
    // A maximum that is no number: the type is stored, then the maximum is the error.
    backend.docs.lock().unwrap()["tune"]["max_telemetry_vars"]["roll"] = json!("x");
    backend.docs.lock().unwrap()["tune"]["motion_effects"]["roll_type"] = json!("rate");
    app.refresh_docs();
    assert!(dispatch(&mut app, "set", "Motion:roll_type", "0").is_err());
    assert_eq!(stored(&backend, &["motion_effects", "roll_type"]), json!("angle"));
    assert_eq!(stored(&backend, &["max_telemetry_vars", "roll"]), json!("x"));
    drop(app.take_threads());
}

/// A driving style's smoothing is rounded to the smoothing sliders' step.
#[test]
fn a_driving_style_smoothing_follows_the_slider_step() {
    for (step, medium) in [(json!(2), 16), (json!(1), 15), (json!(10), 20)] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["features"]["smoothing_slider_step"] = step.clone();
        let mut app = motion(&backend);
        dispatch(&mut app, "set", "Motion:intellitune", "1").unwrap();
        for axis in ["surge", "sway", "traction_loss", "roll", "heave", "pitch", "yaw"] {
            assert_eq!(stored(&backend, &["motion_effect_smoothing_factor", axis]), json!(medium), "{step} {axis}");
        }
        drop(app.take_threads());
    }
}

#[test]
fn hot_keys_step_the_stored_values() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    dispatch(&mut app, "set", "Motion:roll", "10").unwrap();
    dispatch(&mut app, "set", "Motion:motion_max_power", "100").unwrap();
    backend.docs.lock().unwrap()["ui_actions"] = json!(["motion_box_roll_up", "motion_box_motion_max_power_up"]);
    let pending = |action: &str| backend.ui_action_pending(action);
    apply_hotkeys(&mut app, &pending).unwrap();
    assert_eq!(stored(&backend, &["motion_effects", "roll"]), json!(11));
    assert_eq!(stored(&backend, &["motion_max_power"]), json!(100), "a step stays within the range");
    drop(app.take_threads());
}

#[test]
fn compare_lists_the_changed_values() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    app.ui.snapshot = Some(app.docs.tune.clone());
    let before = crate::backend::num(app.tune(&["motion_effects", "heave"]));
    dispatch(&mut app, "set", "Motion:heave", "3").unwrap();
    assert!(crate::ui::dirty(&app));
    assert!(crate::ui::render(&app, Region::Save).contains("Unsaved changes"));
    crate::ui::compare(&mut app);
    let entry = app.ui.notices.history.last().unwrap();
    assert!(
        entry.message.contains(&format!("{}: {} → 3", "Vert G Heave", crate::ui::html::num(before))),
        "{}",
        entry.message
    );
    assert!(app.ui.drawer);
    drop(app.take_threads());
}

#[test]
fn downloaded_profiles_opens_the_library_at_motion() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let html = body(&app);
    assert!(html.contains("data-arg=\"Motion:@library:\"") && html.contains(">Downloaded profiles<"));
    dispatch(&mut app, "action", "Motion:@library:", "").unwrap();
    let motion = super::super::library::category_index(openrigcue_config::synced_profiles::Category::Motion);
    assert_eq!((app.ui.screen, app.ui.section), (Screen::DownloadedProfiles, motion));
    drop(app.take_threads());
}

// ---- Rows, rules and stored values -----------------------------------------------------------------------

/// The settings groups of the page, by their index in the page description.
const PLATFORM: usize = 0;
const EFFECTS: usize = 1;
const RACING: usize = 2;
const FLIGHT: usize = 3;
const SMOOTHING: usize = 4;
const ADVANCED: usize = 5;
const ACTUATORS: usize = 6;
const CONTROLLER: usize = 7;

/// A profile with both axis types, so their rows are offered once it loads.
fn with_axis_types(backend: &FakeBackend) {
    let mut docs = backend.docs.lock().unwrap();
    docs["tune"]["motion_effects"]["roll_type"] = json!("angle");
    docs["tune"]["motion_effects"]["pitch_type"] = json!("angle");
}

/// The body with the settings group `index` selected.
fn group(app: &mut MyApp, index: usize) -> String {
    dispatch(app, "section", "", &index.to_string()).unwrap();
    body(app)
}

/// The markup of one row: from its `data-control` attribute to the next row.
fn row<'a>(html: &'a str, control: &str) -> Option<&'a str> {
    let start = html.find(&format!("data-control=\"{control}\""))?;
    let rest = &html[start..];
    let end = rest[1..].find("data-control=\"").map_or(rest.len(), |i| i + 1);
    Some(&rest[..end])
}

/// The position of a row in the markup.
fn at(html: &str, control: &str) -> usize {
    html.find(&format!("data-control=\"{control}\"")).unwrap_or_else(|| panic!("{control} missing"))
}

fn model_with(test: impl Fn(&crate::catalog::ModelFlags) -> bool) -> &'static crate::catalog::ModelFlags {
    crate::catalog::models().iter().find(|m| test(m)).expect("a model of the catalogue")
}

/// Sets the motion model and reads the documents again.
fn set_model(app: &mut MyApp, backend: &FakeBackend, name: &str) {
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] = json!(name);
    app.refresh_docs();
}

/// The Wind screen's settings group with the Ejection Seat row of its page description.
fn wind_rows(app: &mut MyApp) -> String {
    let section = spec::wind_page()
        .sections
        .iter()
        .position(|s| s.controls.iter().any(|c| c.id == "ejection_seat"))
        .expect("the Ejection Seat group");
    app.ui.section = section;
    let html = crate::ui::screens::wind::main(app);
    app.ui.section = 0;
    html
}

fn tune_patches(backend: &FakeBackend) -> Vec<serde_json::Value> {
    backend.patches("profile")
}

#[test]
fn the_groups_and_rows_keep_the_page_order() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let reaction = model_with(|m| m.show_reaction_speed && !m.name.contains("CANBus"));
    set_model(&mut app, &backend, &reaction.name);
    let html = group(&mut app, PLATFORM);
    let order = ["motion_max_power", "motion_smoothing_factor", "reaction_speed", "motion_boost_factor"];
    assert!(order.windows(2).all(|w| at(&html, w[0]) < at(&html, w[1])), "{html}");
    let full = model_with(|m| m.main_actuators && !STR_MODELS.contains(&m.name.as_str()) && !m.name.contains("CANBus"));
    set_model(&mut app, &backend, &full.name);
    let html = group(&mut app, PLATFORM);
    let order = ["motion_max_power", "motion_max_horizontal", "motion_smoothing_factor", "motion_boost_factor"];
    assert!(order.windows(2).all(|w| at(&html, w[0]) < at(&html, w[1])), "{html}");
    assert!(!html.contains("data-control=\"scn6_speed\""), "no actuator speed rows without SIMTECHREVOLUTION");
    // The groups in page order; there is no "Common" group.
    let picker: Vec<usize> = ["Platform", "Effects", "Racing", "Flight", "Smoothing Effects", "Advanced"]
        .iter()
        .map(|g| html.find(&format!(">{g}</option>")).unwrap_or_else(|| panic!("{g}")))
        .collect();
    assert!(picker.windows(2).all(|w| w[0] < w[1]));
    assert!(!html.contains(">Common</option>"));
    let effects = group(&mut app, EFFECTS);
    assert!(at(&effects, "intellitune") < at(&effects, "pitch") && at(&effects, "pitch") < at(&effects, "engine"));
    let racing = group(&mut app, RACING);
    assert!(at(&racing, "traction_loss") < at(&racing, "abs_lock"));
    assert!(group(&mut app, FLIGHT).contains("data-control=\"afterburner\""));
    let smoothing = group(&mut app, SMOOTHING);
    assert!(at(&smoothing, "motion_smoothing_pitch") < at(&smoothing, "motion_smoothing_traction_loss_front"));
    assert!(row(&smoothing, "motion_smoothing_traction_loss_front").unwrap().contains(">Traction Loss Front<"));
    assert!(row(&smoothing, "motion_smoothing_traction_loss").unwrap().contains(">Traction Loss Rear<"));
    let advanced = group(&mut app, ADVANCED);
    let order = ["min_traction_loss_activation", "min_traction_loss_front_activation", "gear_smoothing_factor"];
    assert!(order.windows(2).all(|w| at(&advanced, w[0]) < at(&advanced, w[1])));
    let actuators = group(&mut app, ACTUATORS);
    assert!(at(&actuators, "motion_main_actuators_sway") < at(&actuators, "motion_main_actuators_traction_loss_front"));
    // The SIMTECHREVOLUTION models (the only ones with the actuator speed rows) are not registered; their
    // values are stored as whole numbers.
    assert!(crate::catalog::models().iter().all(|m| m.manufacturer != "SIMTECHREVOLUTION"));
    dispatch(&mut app, "set", "Motion:scn6_speed", "500").unwrap();
    assert_eq!(stored(&backend, &["scn6_speed"]), json!(500));
    // A THANOS platform has the controller group last.
    let thanos = model_with(|m| m.manufacturer == "THANOS");
    set_model(&mut app, &backend, &thanos.name);
    let controller = group(&mut app, CONTROLLER);
    assert!(controller.contains(">Controller</option>"));
    let order = ["thanos_spike_filter_vertical", "thanos_spike_filter_horizontal", "thanos_alert"];
    assert!(order.windows(2).all(|w| at(&controller, w[0]) < at(&controller, w[1])));
    drop(app.take_threads());
}

#[test]
fn effect_rows_store_their_values_switch_and_maximum() {
    let backend = FakeBackend::new();
    with_axis_types(&backend);
    let mut app = motion(&backend);
    dispatch(&mut app, "set", "Motion:sway", "13").unwrap();
    assert!(tune_patches(&backend).contains(&json!({"motion_effects": {"sway": 13}})));
    assert!(row(&group(&mut app, EFFECTS), "sway")
        .unwrap()
        .contains("value=\"13\" data-act=\"set\" data-arg=\"Motion:sway\""));
    // Off: the slider and the Max Telemetry box are disabled; on again: both are enabled.
    dispatch(&mut app, "set", "Motion:sway:toggle", "0").unwrap();
    assert!(tune_patches(&backend).contains(&json!({"motion_switch": {"sway": false}})));
    let sway = body(&app);
    let sway = row(&sway, "sway").unwrap();
    assert!(sway.contains("data-arg=\"Motion:sway\" data-output") && sway.contains(" disabled"));
    assert!(sway.contains("data-arg=\"Motion:sway:max\" disabled"));
    dispatch(&mut app, "set", "Motion:sway:toggle", "1").unwrap();
    assert!(!row(&body(&app), "sway").unwrap().contains(" disabled"));
    // Pitch follows the rate: its Max Telemetry stores the rate maximum, as an integer.
    backend.docs.lock().unwrap()["tune"]["max_telemetry_vars"]["pitch_rate"] = json!(5);
    app.refresh_docs();
    super::super::reload::profile_load(&mut app).unwrap();
    dispatch(&mut app, "set", "Motion:pitch_type", "1").unwrap();
    dispatch(&mut app, "set", "Motion:pitch:max", "12").unwrap();
    assert_eq!(tune_patches(&backend).last().unwrap(), &json!({"max_telemetry_vars": {"pitch_rate": 12}}));
    assert!(body(&app).contains("value=\"12\" data-act=\"set\" data-arg=\"Motion:pitch:max\""));
    drop(app.take_threads());
}

#[test]
fn an_axis_type_is_angle_or_rate_and_needs_its_maximum() {
    let backend = FakeBackend::new();
    with_axis_types(&backend);
    let mut app = motion(&backend);
    // Neither Angle nor Rate: an error and nothing is stored.
    backend.calls.lock().unwrap().clear();
    for value in ["7", "", "Rate"] {
        assert!(dispatch(&mut app, "set", "Motion:roll_type", value).unwrap_err().starts_with("KeyError"));
    }
    assert!(tune_patches(&backend).is_empty());
    for (choice, kind, maximum) in [("0", "angle", "roll"), ("1", "rate", "roll_rate")] {
        backend.docs.lock().unwrap()["tune"]["max_telemetry_vars"][maximum] = json!(31);
        app.refresh_docs();
        dispatch(&mut app, "set", "Motion:roll_type", choice).unwrap();
        assert_eq!(stored(&backend, &["motion_effects", "roll_type"]), json!(kind));
        let html = group(&mut app, EFFECTS);
        assert!(row(&html, "roll").unwrap().contains("value=\"31\" data-act=\"set\" data-arg=\"Motion:roll:max\""));
        let advanced = group(&mut app, ADVANCED);
        let selected = if kind == "angle" { ">Angle</option>" } else { ">Rate</option>" };
        assert!(row(&advanced, "roll_type").unwrap().contains(&format!("selected{selected}")));
    }
    // Rate without a rate maximum: the type is stored, then the missing maximum is the error.
    backend.docs.lock().unwrap()["tune"]["max_telemetry_vars"].as_object_mut().unwrap().remove("roll_rate");
    backend.docs.lock().unwrap()["tune"]["motion_effects"]["roll_type"] = json!("angle");
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "set", "Motion:roll_type", "1").unwrap_err();
    assert!(error.starts_with("KeyError") && error.contains("roll_rate"), "{error}");
    assert_eq!(tune_patches(&backend), [json!({"motion_effects": {"roll_type": "rate"}})]);
    drop(app.take_threads());
}

#[test]
fn the_spike_filters_store_whole_numbers_and_show_millimetres_of_the_stroke() {
    let backend = FakeBackend::new();
    let thanos = model_with(|m| m.manufacturer == "THANOS");
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] = json!(thanos.name);
    backend.docs.lock().unwrap()["status"]["devices"]["thanos"]["config"] = json!({"klm_mode": 1});
    let mut app = motion(&backend);
    for (stroke, shown) in [(json!(0), None), (json!(100), Some("10.0")), (json!("100.5"), None)] {
        backend.docs.lock().unwrap()["settings"]["thanos"]["actuator_vertical_stroke"] = stroke.clone();
        app.refresh_docs();
        dispatch(&mut app, "set", "Motion:thanos_spike_filter_horizontal", "6553").unwrap();
        assert_eq!(stored(&backend, &["thanos_spike_filter_horizontal"]), json!(6553));
        let html = group(&mut app, CONTROLLER);
        let horizontal = row(&html, "thanos_spike_filter_horizontal").unwrap();
        assert!(horizontal.contains("value=\"6553\""));
        match shown {
            Some(millimetres) => assert!(horizontal.contains(millimetres) && horizontal.contains(" mm<"), "{stroke}"),
            None => assert!(!horizontal.contains(" mm<"), "{stroke}: {horizontal}"),
        }
    }
    dispatch(&mut app, "set", "Motion:thanos_spike_filter_vertical", "330").unwrap();
    assert!(tune_patches(&backend).contains(&json!({"thanos_spike_filter_vertical": 330})));
    drop(app.take_threads());
}

#[test]
fn rows_show_the_stored_values() {
    let backend = FakeBackend::new();
    let thanos = model_with(|m| m.manufacturer == "THANOS");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["motion"]["model"] = json!(thanos.name);
        for (key, value) in [("pitch", 12), ("roll", 8), ("yaw", 3), ("sway", 15)] {
            docs["tune"]["motion_effects"][key] = json!(value);
        }
        for (key, value) in [("pitch", true), ("roll", false), ("yaw", true)] {
            docs["tune"]["motion_switch"][key] = json!(value);
        }
        docs["tune"]["max_telemetry_vars"]["yaw"] = json!(90);
        docs["tune"]["motion_effects"].as_object_mut().unwrap().remove("roll_type");
        docs["tune"]["motion_effects"]["pitch_type"] = json!("angle");
        docs["tune"]["motion_effect_smoothing_factor"]["pitch"] = json!(7);
        docs["tune"]["thanos_spike_filter_vertical"] = json!(300);
        docs["tune"]["intellitune_motion_preference"] = json!("Medium");
    }
    let mut app = motion(&backend);
    let effects = group(&mut app, EFFECTS);
    assert!(row(&effects, "pitch").unwrap().contains("value=\"12\" data-act=\"set\" data-arg=\"Motion:pitch\""));
    let roll = row(&effects, "roll").unwrap();
    assert!(roll.contains("value=\"8\"") && roll.contains(" disabled"), "a switched-off effect is disabled");
    assert!(!row(&effects, "pitch").unwrap().contains(" disabled"));
    assert!(row(&effects, "yaw").unwrap().contains("value=\"90\" data-act=\"set\" data-arg=\"Motion:yaw:max\""));
    assert!(row(&effects, "intellitune").unwrap().contains("selected>Medium</option>"));
    let smoothing = group(&mut app, SMOOTHING);
    assert!(row(&smoothing, "motion_smoothing_pitch").unwrap().contains("value=\"7\""));
    // A missing axis type hides its row; a stored one is shown and selected.
    let advanced = group(&mut app, ADVANCED);
    assert!(row(&advanced, "roll_type").is_none());
    assert!(row(&advanced, "pitch_type").unwrap().contains("selected>Angle</option>"));
    assert!(row(&group(&mut app, CONTROLLER), "thanos_spike_filter_vertical").unwrap().contains("value=\"300\""));
    // The screen reads the documents the main loop keeps current.
    backend.docs.lock().unwrap()["tune"]["thanos_spike_filter_vertical"] = json!(1024);
    backend.docs.lock().unwrap()["tune"]["motion_effects"]["pitch"] = json!(17);
    app.refresh_docs();
    assert!(row(&group(&mut app, CONTROLLER), "thanos_spike_filter_vertical").unwrap().contains("value=\"1024\""));
    assert!(row(&group(&mut app, EFFECTS), "pitch").unwrap().contains("value=\"17\""));
    assert_eq!(stored(&backend, &["motion_effects", "pitch"]), json!(17), "showing a value does not store it");
    drop(app.take_threads());
}

#[test]
fn rows_with_missing_or_unusable_values_still_show() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["max_telemetry_vars"]["pitch"] = json!("bad");
        docs["tune"]["motion_effect_smoothing_factor"].as_object_mut().unwrap().remove("roll");
        docs["tune"]["motion_reaction_speed"] = json!("invalid");
        docs["tune"]["motion_effects"].as_object_mut().unwrap().remove("roll");
        docs["tune"]["intellitune_motion_preference"] = serde_json::Value::Null;
        docs["tune"]["motion_max_power"] = json!(37);
    }
    let mut app = motion(&backend);
    let platform = group(&mut app, PLATFORM);
    assert!(row(&platform, "motion_max_power").unwrap().contains("value=\"37\""));
    assert!(row(&platform, "motion_boost_factor").is_some());
    let effects = group(&mut app, EFFECTS);
    assert!(row(&effects, "pitch").unwrap().contains("value=\"0\" data-act=\"set\" data-arg=\"Motion:pitch:max\""));
    assert!(row(&effects, "roll").unwrap().contains("value=\"0\" data-act=\"set\" data-arg=\"Motion:roll\""));
    assert!(row(&effects, "yaw").is_some(), "the rows after a missing value");
    // A preference that is no choice shows no selection.
    assert!(row(&effects, "intellitune").unwrap().contains("<option value=\"\" selected disabled>"));
    let smoothing = group(&mut app, SMOOTHING);
    assert!(row(&smoothing, "motion_smoothing_roll").unwrap().contains("value=\"0\""));
    assert!(row(&smoothing, "motion_smoothing_traction_loss_front").is_some());
    drop(app.take_threads());
}

/// A catalogue game and a feature column it lacks, among `columns`.
fn game_lacking(columns: &[&str]) -> (String, String) {
    let features = crate::catalog::features();
    features
        .games
        .iter()
        .filter(|(game, _)| openrigcue_config::catalog::Catalog::builtin().game(game).is_some())
        .find_map(|(game, supported)| {
            columns
                .iter()
                .find(|c| features.columns.iter().any(|n| n == **c) && !supported.iter().any(|n| n == **c))
                .map(|c| (game.clone(), (*c).to_owned()))
        })
        .expect("a game without one of the columns")
}

#[test]
fn the_game_feature_rule_hides_or_disables_rows_the_game_lacks() {
    let effects: Vec<&str> = Registry::builtin().motion_effects.iter().map(|e| e.id.as_str()).collect();
    let smoothing = ["sway", "surge", "heave", "traction_loss", "traction_loss_front"];
    let actuators = ["sway", "surge", "traction_loss", "traction_loss_front"];
    let main_actuators = model_with(|m| m.main_actuators && !STR_MODELS.contains(&m.name.as_str()));
    for (columns, prefix, groups) in [
        (&effects[..], "", &[EFFECTS, RACING, FLIGHT][..]),
        (&smoothing[..], "motion_smoothing_", &[SMOOTHING][..]),
        (&actuators[..], "motion_main_actuators_", &[ACTUATORS][..]),
    ] {
        let (game, column) = game_lacking(columns);
        let name = format!("{prefix}{column}");
        let backend = FakeBackend::new();
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["status"]["session"]["game"] = json!(game);
            docs["settings"]["motion"]["model"] = json!(main_actuators.name);
            docs["tune"]["motion_switch"][&column] = json!(true);
        }
        let mut app = loaded(backend.clone());
        navigate(&mut app, Screen::Motion, 0);
        // On: hidden; off: shown and disabled; a value that is no yes or no: the rule does not apply.
        for (flag, hidden, disabled) in
            [(json!(true), true, false), (json!(false), false, true), (json!("on"), false, false)]
        {
            backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = flag.clone();
            app.refresh_docs();
            let html: String = groups.iter().map(|index| group(&mut app, *index)).collect();
            match row(&html, &name) {
                Some(found) => {
                    assert!(!hidden, "{game} {flag} {name} hidden");
                    assert_eq!(
                        found.contains(" disabled"),
                        disabled,
                        "{game} {flag} {name} disabled where the game lacks it"
                    );
                }
                None => assert!(hidden, "{game} {flag} {name} shown"),
            }
        }
        drop(app.take_threads());
    }
}

#[test]
fn the_advanced_rows_follow_the_model_and_the_game() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let with = model_with(|m| m.enhanced_gforces);
    let without = model_with(|m| !m.enhanced_gforces);
    set_model(&mut app, &backend, &with.name);
    assert!(group(&mut app, ADVANCED).contains("data-control=\"enhanced_gforces\""));
    set_model(&mut app, &backend, &without.name);
    assert!(!group(&mut app, ADVANCED).contains("data-control=\"enhanced_gforces\""));
    // A racing game: Autotune, the gear smoothing and both thresholds; the Wind screen has no Ejection Seat.
    let racing = group(&mut app, ADVANCED);
    for name in ["gear_smoothing_factor", "min_traction_loss_activation", "min_traction_loss_front_activation"] {
        assert!(racing.contains(&format!("data-control=\"{name}\"")), "{name}");
    }
    assert!(group(&mut app, EFFECTS).contains("data-control=\"intellitune\""));
    assert!(!wind_rows(&mut app).contains("Wind:ejection_seat\""));
    // Another kind of game: none of them, and the Wind screen offers the Ejection Seat.
    use openrigcue_config::catalog::{Catalog, GameKind};
    let flight = Catalog::builtin().games().iter().find(|g| g.kind != GameKind::Racing).unwrap();
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(flight.id);
    app.refresh_docs();
    app.ui.session = crate::ui::session::read(&*app.backend);
    let other = group(&mut app, ADVANCED);
    for name in ["gear_smoothing_factor", "min_traction_loss_activation", "min_traction_loss_front_activation"] {
        assert!(!other.contains(&format!("data-control=\"{name}\"")), "{name}");
    }
    // The Max Telemetry row stays with its other buttons; only Autotune is for racing games.
    let effects = group(&mut app, EFFECTS);
    let intellitune = row(&effects, "intellitune").expect("the Max Telemetry row");
    assert!(!intellitune.contains(">Autotune<"));
    assert!(intellitune.contains(">Tuning Help<") && intellitune.contains(">Go to Telemetry<"));
    assert!(intellitune.contains("id=\"c-motion-intellitune\" class=\"field-input\""), "the driving style stays");
    let autotune = spec::motion_page(Registry::builtin(), 1.0)
        .control("intellitune")
        .unwrap()
        .buttons
        .iter()
        .position(|b| matches!(b, Widget::Button { run: Action::Intellitune, .. }))
        .unwrap();
    dispatch(&mut app, "action", &format!("Motion:intellitune:{autotune}"), "").unwrap();
    assert!(app.ui.dialog.is_none(), "Autotune does not start outside racing games");
    assert!(wind_rows(&mut app).contains("Wind:ejection_seat\""));
    drop(app.take_threads());
}

#[test]
fn reaction_speed_needs_a_number_and_keeps_the_profile_write_when_the_setting_fails() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let before = stored(&backend, &["motion_reaction_speed"]);
    backend.calls.lock().unwrap().clear();
    for value in ["NaN", "inf", "x"] {
        assert!(dispatch(&mut app, "set", "Motion:reaction_speed", value).unwrap_err().starts_with("ValueError"));
    }
    assert!(tune_patches(&backend).is_empty() && backend.patches("settings").is_empty());
    assert_eq!(stored(&backend, &["motion_reaction_speed"]), before);
    // The platform setting fails after the profile took the value.
    backend.docs.lock().unwrap()["settings_failure"] = json!("TypeError: fixture settings failure");
    let error = dispatch(&mut app, "set", "Motion:reaction_speed", "85").unwrap_err();
    assert_eq!(error, "TypeError: fixture settings failure");
    assert_eq!(stored(&backend, &["motion_reaction_speed"]), json!(85));
    let reaction = model_with(|m| m.show_reaction_speed);
    set_model(&mut app, &backend, &reaction.name);
    assert!(row(&group(&mut app, PLATFORM), "reaction_speed").unwrap().contains("value=\"85\""));
    drop(app.take_threads());
}

#[test]
fn an_unusable_smoothing_step_falls_back_to_one() {
    for (step, expected) in [(json!(5), 5.0), (json!("1.5"), 1.0), (serde_json::Value::Null, 1.0), (json!([]), 1.0)] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["features"]["smoothing_slider_step"] = step.clone();
        let mut app = motion(&backend);
        assert_eq!(smoothing_step(&app), expected, "{step}");
        let html = group(&mut app, SMOOTHING);
        let pitch = row(&html, "motion_smoothing_pitch").unwrap();
        assert!(pitch.contains(&format!("step=\"{}\"", crate::ui::html::num(expected))), "{step}: {pitch}");
        drop(app.take_threads());
    }
}

/// A service whose profile write of one path fails; everything else is the [`FakeBackend`].
struct FailingWrite {
    inner: std::sync::Arc<FakeBackend>,
    pointer: &'static str,
}

impl Backend for FailingWrite {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, crate::backend::Failure> {
        if method == Method::Patch
            && path == "profile"
            && body.as_ref().is_some_and(|b| b.pointer(self.pointer).is_some())
        {
            return Err(crate::backend::Failure { status: 500, detail: "RuntimeError: fixture write failed".into() });
        }
        self.inner.call(method, path, body)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, crate::backend::Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), crate::backend::Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[test]
fn a_failing_write_stops_the_driving_style_and_the_hot_keys() {
    let inner = FakeBackend::new();
    let mut app = motion(&inner);
    app.backend =
        std::sync::Arc::new(FailingWrite { inner: inner.clone(), pointer: "/motion_effects/surge_acceleration" });
    let before = stored(&inner, &["motion_effects", "surge_deceleration"]);
    let error = dispatch(&mut app, "set", "Motion:intellitune", "0").unwrap_err();
    assert_eq!(error, "RuntimeError: fixture write failed");
    assert_eq!(stored(&inner, &["intellitune_motion_preference"]), json!("Soft"), "stored before the effects");
    assert_eq!(stored(&inner, &["motion_effects", "surge_deceleration"]), before, "the later effects stay");
    // A hot key whose write fails: the later keys of the pass are not applied.
    app.backend = std::sync::Arc::new(FailingWrite { inner: inner.clone(), pointer: "/motion_effects/pitch" });
    let roll = stored(&inner, &["motion_effects", "roll"]);
    inner.docs.lock().unwrap()["ui_actions"] = json!(["motion_box_pitch_up", "motion_box_roll_up"]);
    let pending = |action: &str| inner.ui_action_pending(action);
    assert_eq!(apply_hotkeys(&mut app, &pending).unwrap_err(), "RuntimeError: fixture write failed");
    assert_eq!(stored(&inner, &["motion_effects", "roll"]), roll);
    drop(app.take_threads());
}

#[test]
fn a_driving_style_is_stored_then_sets_the_effects_and_their_smoothing() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let front = stored(&backend, &["motion_effect_smoothing_factor", "traction_loss_front"]);
    let front_effect = stored(&backend, &["motion_effects", "traction_loss_front"]);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Motion:intellitune", "0").unwrap();
    assert_eq!(tune_patches(&backend)[0], json!({"intellitune_motion_preference": "Soft"}), "the choice first");
    for effect in ["surge_acceleration", "surge_deceleration", "sway", "traction_loss", "roll", "heave", "pitch"] {
        let value = stored(&backend, &["motion_effects", effect]);
        assert!(value.as_i64().is_some_and(|v| (10..=12).contains(&v)), "{effect} {value}");
    }
    assert_eq!(stored(&backend, &["motion_effects", "yaw"]), json!(0));
    for axis in ["surge", "sway", "traction_loss", "roll", "heave", "pitch", "yaw"] {
        assert_eq!(stored(&backend, &["motion_effect_smoothing_factor", axis]), json!(20), "{axis}");
    }
    assert_eq!(stored(&backend, &["motion_effect_smoothing_factor", "traction_loss_front"]), front);
    assert_eq!(stored(&backend, &["motion_effects", "traction_loss_front"]), front_effect);
    // Medium.
    dispatch(&mut app, "set", "Motion:intellitune", "1").unwrap();
    assert_eq!(stored(&backend, &["motion_effect_smoothing_factor", "pitch"]), json!(15));
    assert!(stored(&backend, &["motion_effects", "pitch"]).as_i64().is_some_and(|v| (14..=16).contains(&v)));
    // No such style: an error before anything is stored.
    backend.calls.lock().unwrap().clear();
    assert!(dispatch(&mut app, "set", "Motion:intellitune", "3").unwrap_err().starts_with("KeyError"));
    assert!(dispatch(&mut app, "set", "Motion:intellitune", "x").unwrap_err().starts_with("ValueError"));
    assert!(tune_patches(&backend).is_empty());
    drop(app.take_threads());
}

#[test]
fn driving_style_values_come_from_the_shared_random_sequence() {
    use openrigcue_cueing::{python_random, EffectRng, SharedRandom};
    const CHILD: &str = "OPENRIGCUE_MOTION_RANDOM_SEQUENCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // The sequence is shared by the whole process: the check runs alone in its own process.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ui::screens::motion::tests::driving_style_values_come_from_the_shared_random_sequence",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "isolated shared random sequence failed");
        return;
    }
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let mut replay = python_random().lock().unwrap().clone();
    driving_style(&mut app, "Aggressive").unwrap();
    for effect in ["surge_acceleration", "surge_deceleration", "sway", "traction_loss", "roll", "heave", "pitch"] {
        assert_eq!(
            i128::from(stored(&backend, &["motion_effects", effect]).as_i64().unwrap()),
            replay.randint(17, 19),
            "{effect}"
        );
    }
    assert_eq!(SharedRandom.randint(1, 1 << 30), replay.randint(1, 1 << 30), "the cueing draws continue it");
    drop(app.take_threads());
}

#[test]
fn stored_values_convert_like_the_service() {
    use crate::backend::native_int;
    assert_eq!(native_int(&LegacyValue::Float(12.9)).unwrap(), 12);
    assert_eq!(native_int(&LegacyValue::from(" 7 ")).unwrap(), 7);
    assert!(native_int(&LegacyValue::from("7.5")).unwrap_err().starts_with("ValueError:"));
    assert_eq!(openrigcue_model::python_repr::str_value(&LegacyValue::Float(20.0)), "20.0");
    assert_eq!(openrigcue_model::python_repr::str_value(&LegacyValue::Float(1.5)), "1.5");
    // The rows a value of the page is written from: whole numbers stay whole.
    assert_eq!(read::number("12").unwrap(), LegacyValue::Int(12));
    assert_eq!(read::number("1.5").unwrap(), LegacyValue::Float(1.5));
    let page = spec::motion_page(Registry::builtin(), 1.0);
    assert_eq!(page.control("motion_smoothing_traction_loss_front").unwrap().label, "Traction Loss Front");
    assert_eq!(page.control("motion_smoothing_traction_loss").unwrap().label, "Traction Loss Rear");
}

// ---- Yaw VR, controller and Hardware rules ---------------------------------------------------------------

#[test]
fn the_rows_follow_the_yaw_vr_platform() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    let default = crate::catalog::model("None").unwrap();
    let platform = group(&mut app, PLATFORM);
    assert!(!platform.contains(">Move platform to center</button>"));
    assert_eq!(platform.contains("data-control=\"reaction_speed\""), default.show_reaction_speed);
    let yaw = model_with(|m| m.manufacturer == "Yaw VR");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["motion"]["model"] = json!(yaw.name);
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
        docs["status"]["devices"]["yaw_vr"] = json!({"limits": {"yaw_limit_enabled": 0}});
    }
    // The rules read the service's current values.
    let platform = group(&mut app, PLATFORM);
    assert!(platform.contains(">Move platform to center</button>"), "a connected Yaw VR can be centred");
    assert_eq!(platform.contains("data-control=\"reaction_speed\""), yaw.show_reaction_speed);
    // The yaw limit is off: the Yaw row, its Max Telemetry and its smoothing are disabled.
    let effects = group(&mut app, EFFECTS);
    let row_yaw = row(&effects, "yaw").unwrap();
    assert!(row_yaw.contains("data-arg=\"Motion:yaw\" data-output") && row_yaw.contains(" disabled"));
    assert!(row_yaw.contains("data-arg=\"Motion:yaw:max\" disabled"));
    assert!(!row(&effects, "pitch").unwrap().contains(" disabled"));
    assert!(row(&group(&mut app, SMOOTHING), "motion_smoothing_yaw").unwrap().contains(" disabled"));
    // The text "0" is not the number 0; a limit that is on enables the rows.
    for limit in [json!("0"), json!(1)] {
        backend.docs.lock().unwrap()["status"]["devices"]["yaw_vr"] = json!({"limits": {"yaw_limit_enabled": limit}});
        assert!(!row(&group(&mut app, EFFECTS), "yaw").unwrap().contains(" disabled"), "{limit}");
        assert!(!row(&group(&mut app, SMOOTHING), "motion_smoothing_yaw").unwrap().contains(" disabled"));
    }
    // Not connected: no Center.
    backend.docs.lock().unwrap()["status"]["motion"]["link"]["status"] = json!("Not Connected");
    assert!(!group(&mut app, PLATFORM).contains(">Move platform to center</button>"));
    // A model the catalogue does not know (or no model text): no model rules, and the screen still shows.
    for model in [Some(json!("missing-model")), Some(json!([])), None] {
        match &model {
            Some(model) => backend.docs.lock().unwrap()["settings"]["motion"]["model"] = model.clone(),
            None => {
                drop(backend.docs.lock().unwrap()["settings"]["motion"].as_object_mut().unwrap().shift_remove("model"))
            }
        }
        backend.docs.lock().unwrap()["status"]["motion"]["link"]["status"] = json!("Connected");
        backend.docs.lock().unwrap()["status"]["devices"]["yaw_vr"] = json!({"limits": {"yaw_limit_enabled": 0}});
        let platform = group(&mut app, PLATFORM);
        assert!(
            platform.contains("data-control=\"motion_max_power\"")
                && !platform.contains("data-control=\"reaction_speed\"")
        );
        assert!(!platform.contains(">Move platform to center</button>"), "{model:?}");
        assert!(!row(&group(&mut app, EFFECTS), "yaw").unwrap().contains(" disabled"), "{model:?}");
    }
    drop(app.take_threads());
}

/// A service whose read of the motion parameter `parameter` fails.
struct FailingParameter {
    inner: std::sync::Arc<FakeBackend>,
    parameter: &'static str,
}

impl Backend for FailingParameter {
    fn motion_parameter(&self, name: &str) -> Result<LegacyValue, crate::backend::Failure> {
        if name == self.parameter {
            return Err(crate::backend::Failure { status: 500, detail: format!("KeyError: '{name}'") });
        }
        self.inner.motion_parameter(name)
    }
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, crate::backend::Failure> {
        self.inner.call(method, path, body)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, crate::backend::Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), crate::backend::Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[test]
fn failing_platform_reads_show_the_rows_without_their_rules() {
    let inner = FakeBackend::new();
    let yaw = model_with(|m| m.manufacturer == "Yaw VR");
    {
        let mut docs = inner.docs.lock().unwrap();
        docs["settings"]["motion"]["model"] = json!(yaw.name);
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
        docs["status"]["devices"]["yaw_vr"] = json!({"limits": {"yaw_limit_enabled": 0}});
    }
    let mut app = motion(&inner);
    // The motion status cannot be read: no Center, the rows still show.
    app.backend = std::sync::Arc::new(FailingParameter { inner: inner.clone(), parameter: "motion_status" });
    let platform = group(&mut app, PLATFORM);
    assert!(
        platform.contains("data-control=\"motion_max_power\"")
            && !platform.contains(">Move platform to center</button>")
    );
    // The yaw limit cannot be read: the Yaw rows stay enabled.
    app.backend = std::sync::Arc::new(FailingParameter { inner: inner.clone(), parameter: "yaw_limit_enabled" });
    assert!(!row(&group(&mut app, EFFECTS), "yaw").unwrap().contains(" disabled"));
    // The controller configuration cannot be read: the controller rows are disabled with the firmware note.
    let thanos = model_with(|m| m.manufacturer == "THANOS");
    set_model(&mut app, &inner, &thanos.name);
    inner.docs.lock().unwrap()["status"]["devices"]["thanos"]["config"] = json!({"klm_mode": 1});
    app.backend = std::sync::Arc::new(FailingParameter { inner: inner.clone(), parameter: "thanos_config" });
    let controller = group(&mut app, CONTROLLER);
    assert!(row(&controller, "thanos_spike_filter_vertical").unwrap().contains(" disabled"));
    assert!(controller.contains("data-control=\"thanos_alert\""));
    drop(app.take_threads());
}

#[test]
fn the_controller_rows_follow_the_thanos_configuration() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    assert!(!group(&mut app, PLATFORM).contains(">Controller</option>"), "only for THANOS");
    let thanos = model_with(|m| m.manufacturer == "THANOS");
    set_model(&mut app, &backend, &thanos.name);
    for (config, enabled) in [(json!({}), false), (serde_json::Value::Null, true), (json!({"klm_mode": 1}), true)] {
        backend.docs.lock().unwrap()["status"]["devices"]["thanos"]["config"] = config.clone();
        let html = group(&mut app, CONTROLLER);
        assert!(html.contains(">Controller</option>"));
        for name in ["thanos_spike_filter_vertical", "thanos_spike_filter_horizontal"] {
            assert_eq!(!row(&html, name).unwrap().contains(" disabled"), enabled, "{config} {name}");
        }
        // The firmware note shows until the controller has a configuration.
        assert_eq!(html.contains("data-control=\"thanos_alert\""), !enabled, "{config}");
    }
    drop(app.take_threads());
}

#[test]
fn the_hardware_buttons_follow_the_platform_the_game_and_the_test() {
    let backend = FakeBackend::new();
    let yaw = model_with(|m| m.manufacturer == "Yaw VR");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["motion"]["model"] = json!(yaw.name);
        docs["status"]["motion"]["link"]["status"] = json!("Connected");
    }
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Hardware, 0);
    let motion_group = spec::hardware_page().sections.iter().position(|s| s.title == "Motion").unwrap();
    let hardware = |app: &mut MyApp| {
        dispatch(app, "section", "", &motion_group.to_string()).unwrap();
        crate::ui::screens::hardware::main(app)
    };
    // A connected Yaw VR: its settings and Center; the axis assignment only for a model that has one.
    let html = hardware(&mut app);
    assert!(
        html.contains("data-arg=\"Hardware:motion_model:1\"") && html.contains("data-arg=\"Hardware:motion_model:3\"")
    );
    assert_eq!(html.contains("data-arg=\"Hardware:motion_model:0\""), yaw.axis_assignment);
    assert!(!html.contains("data-arg=\"Hardware:motion_test:\" disabled"), "no game runs");
    // A running game (read at once from the service) stops the motion test from starting.
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("emulator");
    assert!(hardware(&mut app).contains("data-arg=\"Hardware:motion_test:\" disabled"));
    // A Thanos4U controller: its settings are locked while a game or the motion test runs.
    let thanos = model_with(|m| m.manufacturer == "THANOS");
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["motion"]["model"] = json!(thanos.name);
        docs["status"]["devices"]["thanos"]["controller"] = json!("Thanos4U(M)");
    }
    assert!(hardware(&mut app).contains("data-arg=\"Hardware:motion_model:2\" disabled>"));
    backend.docs.lock().unwrap()["status"]["session"]["game"] = serde_json::Value::Null;
    assert!(!hardware(&mut app).contains("data-arg=\"Hardware:motion_model:2\" disabled>"));
    backend.docs.lock().unwrap()["status"]["controls"]["tests"]["motion_test"] = json!(true);
    assert!(hardware(&mut app).contains("data-arg=\"Hardware:motion_model:2\" disabled>"));
    drop(app.take_threads());
}

/// A service whose motion status read and centring request answer as given.
struct CenterService {
    inner: std::sync::Arc<FakeBackend>,
    status: Result<LegacyValue, crate::backend::Failure>,
    fail_request: bool,
    events: Mutex<Vec<&'static str>>,
}

impl Backend for CenterService {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, crate::backend::Failure> {
        if path == "motion/center" {
            self.events.lock().unwrap().push("request");
            if self.fail_request {
                return Err(crate::backend::Failure { status: 500, detail: "KeyError: 'motion_homing'".into() });
            }
        }
        self.inner.call(method, path, body)
    }
    fn motion_parameter(&self, name: &str) -> Result<LegacyValue, crate::backend::Failure> {
        if name == "motion_status" {
            self.events.lock().unwrap().push("status");
            return self.status.clone();
        }
        self.inner.motion_parameter(name)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, crate::backend::Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), crate::backend::Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[test]
fn center_reads_the_status_then_asks_a_connected_yaw_vr_from_both_screens() {
    let failure = |detail: &str| crate::backend::Failure { status: 500, detail: detail.into() };
    let page = spec::motion_page(Registry::builtin(), 1.0);
    let header = page.header.iter().position(|b| matches!(b, Widget::Button { run: Action::YawVrCenter, .. })).unwrap();
    for button in [format!("Motion:@header:{header}"), "Hardware:motion_model:3".to_owned()] {
        for (status, fail_request, requested, error) in [
            (Ok(LegacyValue::from("Not Connected")), false, false, None),
            (Ok(LegacyValue::from("Connected")), false, true, None),
            (Ok(LegacyValue::from("Connected")), true, true, Some("KeyError: 'motion_homing'")),
            (Err(failure("KeyError: 'motion_status'")), false, false, Some("KeyError: 'motion_status'")),
        ] {
            let inner = FakeBackend::new();
            let mut app = motion(&inner);
            let service = std::sync::Arc::new(CenterService {
                inner: inner.clone(),
                status,
                fail_request,
                events: Mutex::new(Vec::new()),
            });
            app.backend = service.clone();
            let result = dispatch(&mut app, "action", &button, "");
            assert_eq!(result.err().as_deref(), error, "{button}");
            let events = service.events.lock().unwrap().clone();
            assert_eq!(events.contains(&"request"), requested, "{button} {events:?}");
            if requested {
                let request = events.iter().position(|e| *e == "request").unwrap();
                assert_eq!(events[request - 1], "status", "the status is read first");
            }
            let shown = app.ui.notices.history.iter().filter(|n| n.message == "YawVR was told to center").count();
            assert_eq!(shown, usize::from(error.is_none()), "{button}");
            drop(app.take_threads());
        }
    }
}

// ---- Autotune --------------------------------------------------------------------------------------------

/// Samples of every channel Autotune reads.
fn samples(values: Value) -> LegacyValue {
    let channels = [
        "lateral_acceleration",
        "vertical_acceleration",
        "longitudinal_acceleration",
        "yaw",
        "roll",
        "pitch",
        "lateral_velocity",
    ];
    LegacyValue::from(Value::Object(channels.iter().map(|c| ((*c).to_owned(), values.clone())).collect()))
}

/// A service with Autotune samples that records the wizard's service work: each event's name and whether the
/// application was free at that moment. `fail` names the event that fails; `pause` holds the duration read.
struct WizardService {
    inner: std::sync::Arc<FakeBackend>,
    app: Mutex<Weak<Mutex<MyApp>>>,
    samples: LegacyValue,
    fail: Option<&'static str>,
    events: Mutex<Vec<(String, bool)>>,
    pause: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl WizardService {
    fn new(inner: std::sync::Arc<FakeBackend>, fail: Option<&'static str>) -> std::sync::Arc<WizardService> {
        std::sync::Arc::new(WizardService {
            inner,
            app: Mutex::new(Weak::new()),
            samples: samples(json!([3.0, 4.0])),
            fail,
            events: Mutex::new(Vec::new()),
            pause: Mutex::new(None),
        })
    }

    fn event(&self, name: &str) -> Result<(), crate::backend::Failure> {
        let available = self.app.lock().unwrap().upgrade().is_some_and(|app| app.try_lock().is_ok());
        self.events.lock().unwrap().push((name.to_owned(), available));
        if name == "duration" {
            if let Some((entered, release)) = self.pause.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        if self.fail == Some(name) {
            return Err(crate::backend::Failure { status: 500, detail: format!("RuntimeError: {name} failed") });
        }
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        self.events.lock().unwrap().iter().map(|(name, _)| name.clone()).collect()
    }
}

impl Backend for WizardService {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, crate::backend::Failure> {
        match (method, path) {
            (Method::Delete, "intellitune") => self.event("clear")?,
            (Method::Patch, "controls") => {
                if let Some(on) = body.as_ref().and_then(|b| b.pointer("/telemetry/intellitune_capture")) {
                    self.event(if on == &json!(true) { "capture-on" } else { "capture-off" })?;
                }
            }
            (Method::Post, clip) if clip.starts_with("voice/clips/") => self.event(clip)?,
            _ => {}
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, crate::backend::Failure> {
        if key == "intelli_tune_capture_secs" {
            self.event("duration")?;
        }
        self.inner.native_setting(section, key)
    }
    fn intellitune_samples(&self) -> Result<LegacyValue, crate::backend::Failure> {
        Ok(self.samples.clone())
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, crate::backend::Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), crate::backend::Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The Motion screen of a running catalogue game, its Autotune at step 2 before the count-down, shared like the
/// interface shares it.
fn wizard_app(
    duration: Value,
    fail: Option<&'static str>,
) -> (std::sync::Arc<Mutex<MyApp>>, std::sync::Arc<WizardService>, std::sync::Arc<FakeBackend>) {
    let inner = FakeBackend::new();
    inner.docs.lock().unwrap()["status"]["session"]["game"] = json!("emulator");
    inner.docs.lock().unwrap()["settings"]["motion"]["intellitune_capture_seconds"] = duration;
    let mut app = loaded(inner.clone());
    navigate(&mut app, Screen::Motion, 0);
    // The emulator is no racing game, so the screen offers no Autotune button: the wizard is started directly.
    start_wizard(&mut app).unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(matches!(app.ui.dialog, Some(Dialog::Wizard(WizardStep::Countdown(None)))));
    drop(app.take_threads());
    let service = WizardService::new(inner.clone(), fail);
    app.backend = service.clone();
    let shared = std::sync::Arc::new(Mutex::new(app));
    *service.app.lock().unwrap() = std::sync::Arc::downgrade(&shared);
    (shared, service, inner)
}

fn step_of(shared: &Mutex<MyApp>) -> Option<WizardStep> {
    match &crate::host::lock(shared).ui.dialog {
        Some(Dialog::Wizard(step)) => Some(step.clone()),
        _ => None,
    }
}

#[test]
fn autotune_counts_down_captures_off_the_application_and_opens_the_review() {
    let (shared, service, inner) = wizard_app(json!(1), None);
    let weak = std::sync::Arc::downgrade(&shared);
    std::thread::spawn(move || wizard_timer(weak)).join().unwrap();
    let mut expected = vec!["voice/clips/intellitune_starting?wait=true".to_owned()];
    expected.extend((1..=10).rev().map(|n| format!("voice/clips/{n}")));
    expected.extend(
        ["clear", "capture-on", "duration", "capture-off", "voice/clips/intellitune_captured"].map(String::from),
    );
    assert_eq!(service.names(), expected);
    for (name, available) in service.events.lock().unwrap().iter() {
        assert!(available, "{name}: the service is asked without holding the application");
    }
    assert_eq!(inner.docs.lock().unwrap()["status"]["controls"]["telemetry"]["intellitune_capture"], json!(false));
    // Step 3 opens by itself with Medium chosen and every Autotune effect's suggestion.
    let Some(WizardStep::Review { rows, preference }) = step_of(&shared) else { panic!("review") };
    assert_eq!(preference, 1);
    let effects: Vec<&str> = rows.iter().map(|(effect, ..)| effect.as_str()).collect();
    assert_eq!(effects.len(), 8, "{effects:?}");
    assert!(!effects.contains(&"wheel_slip") && !effects.contains(&"rumble_strips"));
    let html = crate::ui::render(&crate::host::lock(&shared), Region::Dialog);
    assert!(
        html.contains("Autotune - Step 3/3") && html.contains("Suggested Max") && html.contains("selected>Medium<")
    );
    drop(crate::host::lock(&shared).take_threads());
}

#[test]
fn autotune_capture_failures_stop_before_the_next_step() {
    let scenarios: Vec<(Value, Option<&'static str>, &[&str])> = vec![
        (json!(180), Some("clear"), &["clear"]),
        (json!(180), Some("capture-on"), &["clear", "capture-on"]),
        (json!("1.5"), None, &["clear", "capture-on", "duration"]),
        (Value::Null, None, &["clear", "capture-on", "duration"]),
    ];
    let runs: Vec<_> = scenarios
        .into_iter()
        .map(|(duration, fail, expected)| {
            let (shared, service, _) = wizard_app(duration, fail);
            let weak = std::sync::Arc::downgrade(&shared);
            (std::thread::spawn(move || wizard_timer(weak)), shared, service, expected)
        })
        .collect();
    // A duration in other digits with a separator: the capture runs for it.
    let (shared, service, inner) = wizard_app(json!("١_٢٠"), None);
    let weak = std::sync::Arc::downgrade(&shared);
    let running = std::thread::spawn(move || wizard_timer(weak));
    let started = std::time::Instant::now();
    while step_of(&shared) != Some(WizardStep::Capture(120)) {
        assert!(started.elapsed() < Duration::from_secs(20), "the capture starts");
        std::thread::sleep(Duration::from_millis(20));
    }
    {
        let mut app = crate::host::lock(&shared);
        let html = crate::ui::render(&app, Region::Dialog);
        assert!(html.contains("Time Left 02:00") && html.contains("data-act=\"dialog-ok\" disabled"), "{html}");
        dispatch(&mut app, "dialog-close", "", "").unwrap();
    }
    running.join().unwrap();
    let names = service.names();
    let capture = names.iter().position(|n| n == "capture-on").unwrap();
    assert!(names.iter().position(|n| n == "clear").unwrap() < capture);
    assert!(capture < names.iter().position(|n| n == "duration").unwrap());
    assert_eq!(inner.docs.lock().unwrap()["status"]["controls"]["telemetry"]["intellitune_capture"], json!(false));
    for (thread, shared, service, expected) in runs {
        thread.join().unwrap();
        let names: Vec<String> = service.names().into_iter().filter(|n| !n.starts_with("voice/")).collect();
        assert_eq!(names, expected);
        // The window stays at the end of the count-down: no capture is shown and Next stays off.
        assert_eq!(step_of(&shared), Some(WizardStep::Countdown(Some(1))), "{expected:?}");
        drop(crate::host::lock(&shared).take_threads());
    }
}

#[test]
fn autotune_can_be_cancelled_while_the_capture_duration_is_read() {
    let (shared, service, inner) = wizard_app(json!(180), None);
    let (entered, waiting) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    *service.pause.lock().unwrap() = Some((entered, released));
    let weak = std::sync::Arc::downgrade(&shared);
    let running = std::thread::spawn(move || wizard_timer(weak));
    waiting.recv_timeout(Duration::from_secs(20)).unwrap();
    {
        let mut app = shared.try_lock().expect("the application is free while the duration is read");
        dispatch(&mut app, "dialog-close", "", "").unwrap();
    }
    release.send(()).unwrap();
    running.join().unwrap();
    assert_eq!(step_of(&shared), None);
    let names: Vec<String> = service.names().into_iter().filter(|n| !n.starts_with("voice/")).collect();
    assert_eq!(names, ["clear", "capture-on", "duration", "clear", "capture-off"], "nothing after the cancel");
    assert_eq!(inner.docs.lock().unwrap()["status"]["controls"]["telemetry"]["intellitune_capture"], json!(false));
    drop(crate::host::lock(&shared).take_threads());
}

#[test]
fn cancelling_autotune_clears_the_samples_then_ends_the_capture() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    app.ui.dialog = Some(Dialog::Wizard(WizardStep::Capture(30)));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    let calls: Vec<(Method, String)> = backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, p, _)| (*m == Method::Delete && p == "intellitune") || (*m == Method::Patch && p == "controls"))
        .map(|(m, p, _)| (*m, p.clone()))
        .collect();
    assert_eq!(calls, [(Method::Delete, "intellitune".to_owned()), (Method::Patch, "controls".to_owned())]);
    assert!(backend.patches("controls").contains(&json!({"telemetry": {"intellitune_capture": false}})));
    // A failing clear: the window is closed, the error is reported and the capture is not switched.
    let service = WizardService::new(backend.clone(), Some("clear"));
    app.backend = service.clone();
    app.ui.dialog = Some(Dialog::Wizard(WizardStep::Countdown(Some(4))));
    assert_eq!(dispatch(&mut app, "dialog-close", "", "").unwrap_err(), "RuntimeError: clear failed");
    assert!(app.ui.dialog.is_none());
    assert_eq!(service.names(), ["clear"]);
    drop(app.take_threads());
}

#[test]
fn autotune_starts_only_for_the_running_catalogue_game() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    // The service's game now, not the one the screen read before.
    backend.docs.lock().unwrap()["status"]["session"]["game"] = serde_json::Value::Null;
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "action", "Motion:intellitune:1", "").unwrap_err();
    assert_eq!(error, "KeyError: 'none'");
    assert!(app.ui.dialog.is_none());
    let calls = backend.calls.lock().unwrap();
    assert!(calls.iter().any(|(m, p, _)| *m == Method::Post && p == "voice/clips/intellitune_welcome"));
    drop(calls);
    drop(app.take_threads());
}

/// The app with a running game and the samples `samples`.
fn suggesting(samples: LegacyValue) -> (MyApp, std::sync::Arc<FakeBackend>) {
    let inner = FakeBackend::new();
    {
        let mut docs = inner.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("emulator");
        docs["tune"]["motion_effects"]["pitch_type"] = json!("angle");
        docs["tune"]["motion_effects"]["roll_type"] = json!("angle");
    }
    let mut app = loaded(inner.clone());
    navigate(&mut app, Screen::Motion, 0);
    drop(app.take_threads());
    app.backend = std::sync::Arc::new(WizardService {
        inner: inner.clone(),
        app: Mutex::new(Weak::new()),
        samples,
        fail: None,
        events: Mutex::new(Vec::new()),
        pause: Mutex::new(None),
    });
    (app, inner)
}

/// The factory maximum of `effect` for the test game.
fn factory_max(effect: &str) -> f64 {
    crate::backend::num(&crate::catalog::factory_tune("emulator").unwrap()["max_telemetry_vars"][effect]).abs()
}

#[test]
fn the_autotune_review_suggests_from_the_samples_and_applies_them_in_any_language() {
    let (mut app, inner) = suggesting(samples(json!([3.0, 4.0])));
    inner.docs.lock().unwrap()["tune"]["max_telemetry_vars"]["pitch"] = json!(7.5);
    inner.docs.lock().unwrap()["translations"] = json!({"Pitch": "Nicken", "Roll": "Rollen"});
    let rows = suggestions(&app).unwrap();
    assert_eq!(rows.len(), 8);
    let (_, current, suggested) = rows.iter().find(|(e, ..)| e == "pitch").unwrap();
    assert_eq!(current, "7.5", "the current maximum as stored");
    // The mean 3.5 rounds to 4, then the average with the factory maximum, ties to even, at least 1.
    assert_eq!(*suggested, (((4.0 + factory_max("pitch")) / 2.0).round_ties_even() as i64).max(1));
    app.ui.dialog = Some(Dialog::Wizard(WizardStep::Review { rows: rows.clone(), preference: 1 }));
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains(">Nicken<") && html.contains(">Rollen<") && !html.contains(">pitch<"), "{html}");
    // The preference is chosen, then Apply stores every maximum (by effect, whatever the language) and it.
    dispatch(&mut app, "dialog-field", "preference", "2").unwrap();
    inner.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    for (effect, _, suggested) in &rows {
        let write = crate::backend::patch_for(&["max_telemetry_vars", effect], json!(suggested));
        assert!(inner.patches("profile").contains(&write), "{effect}");
    }
    assert_eq!(stored(&inner, &["intellitune_motion_preference"]), json!("Aggressive"));
    assert!(app.ui.dialog.is_none());
    assert!(app.ui.notices.history.iter().any(|n| n.message == "Autotune complete"));
    // A missing current maximum is an error before step 3.
    inner.docs.lock().unwrap()["tune"]["max_telemetry_vars"].as_object_mut().unwrap().remove("heave");
    assert!(suggestions(&app).unwrap_err().starts_with("KeyError"));
    drop(app.take_threads());
}

#[test]
fn the_autotune_mean_keeps_its_precision_and_stops_at_unusable_samples() {
    for (values, mean) in [
        (json!([1e16, 1.0, 1.0, 1.0]), 2_500_000_000_000_001_i64),
        (json!([1e16, 1, 1.0, 1.0]), 2_500_000_000_000_000),
        (json!([10_000_000_000_000_000_i64, 1.0, 1.0, 1.0, 1.0]), 2_000_000_000_000_001),
        (json!([2, 3]), 2),
    ] {
        let mut set = samples(json!([3.0]));
        set.assign_item("pitch", LegacyValue::from(values.clone())).unwrap();
        let (mut app, _) = suggesting(set);
        let rows = suggestions(&app).unwrap();
        let (_, _, suggested) = rows.iter().find(|(e, ..)| e == "pitch").unwrap();
        assert_eq!(
            *suggested,
            (((mean as f64 + factory_max("pitch")) / 2.0).round_ties_even() as i64).max(1),
            "{values}"
        );
        drop(app.take_threads());
    }
    for (values, class) in [
        (LegacyValue::from(json!([null])), "TypeError:"),
        (LegacyValue::from(json!(["3"])), "TypeError:"),
        (LegacyValue::List(vec![]), "ZeroDivisionError:"),
        (LegacyValue::List(vec![LegacyValue::Float(f64::NAN)]), "ValueError:"),
        (LegacyValue::List(vec![LegacyValue::Float(f64::INFINITY)]), "OverflowError:"),
    ] {
        let mut set = samples(json!([3.0]));
        set.assign_item("pitch", values).unwrap();
        let (mut app, _) = suggesting(set);
        let error = suggestions(&app).unwrap_err();
        assert!(error.starts_with(class), "{error}");
        drop(app.take_threads());
    }
}

#[test]
fn a_profile_load_sets_the_platform_reaction_speed_from_the_profile() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] =
        json!(crate::catalog::models().iter().find(|m| m.name != "None").unwrap().name.clone());
    let mut app = motion(&backend);
    for (profile, expected) in [(json!(85), 85), (json!(120), 100), (json!(40), 70), (json!("90"), 90)] {
        backend.docs.lock().unwrap()["tune"]["motion_reaction_speed"] = profile.clone();
        app.refresh_docs();
        super::super::reload::profile_load(&mut app).unwrap();
        assert_eq!(stored(&backend, &["motion_reaction_speed"]), json!(expected), "the profile keeps {profile}");
        assert_eq!(backend.docs.lock().unwrap()["settings"]["motion"]["reaction_speed"], json!(expected), "{profile}");
    }
    // A profile without a usable value changes neither.
    backend.docs.lock().unwrap()["tune"]["motion_reaction_speed"] = json!("fast");
    app.refresh_docs();
    super::super::reload::profile_load(&mut app).unwrap();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["motion"]["reaction_speed"], json!(90));
    // The main loop runs the rule after each profile load.
    backend.docs.lock().unwrap()["tune"]["motion_reaction_speed"] = json!(77);
    backend.docs.lock().unwrap()["status"]["profile"]["generation"] = json!(9);
    app.main_loop_pass();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["motion"]["reaction_speed"], json!(77));
    drop(app.take_threads());
}

#[test]
fn the_refresh_rate_is_kept_within_its_range_at_start() {
    for (stored_rate, expected) in [(json!(5000), json!(1000)), (json!(10), json!(30)), (json!(500), json!(500))] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["settings"]["motion"]["refresh_rate"] = stored_rate.clone();
        let mut app = motion(&backend);
        start(&mut app).unwrap();
        assert_eq!(backend.docs.lock().unwrap()["settings"]["motion"]["refresh_rate"], expected, "{stored_rate}");
        drop(app.take_threads());
    }
}

/// The opening tag of the element with `id` in `html`.
fn tag<'a>(html: &'a str, id: &str) -> &'a str {
    let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element {id}"));
    let start = html[..start].rfind('<').unwrap();
    let end = start + html[start..].find('>').unwrap();
    &html[start..=end]
}

/// The index of a button of a row of the page description.
fn button_index(control: &str, run: impl Fn(&Action) -> bool) -> usize {
    spec::motion_page(Registry::builtin(), 1.0)
        .control(control)
        .unwrap()
        .buttons
        .iter()
        .position(|b| matches!(b, Widget::Button { run: action, .. } if run(action)))
        .unwrap()
}

#[test]
fn hidden_and_disabled_rows_take_no_change() {
    // Stand-by: every part of every row is disabled, and nothing sent for them is taken.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["motion_switch"]["sway"] = json!(true);
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Motion, 0);
    let effects = group(&mut app, EFFECTS);
    assert!(tag(&effects, "c-motion-sway-toggle").contains(" disabled"));
    assert!(tag(&effects, "c-motion-sway-max").contains(" disabled"));
    let help = button_index("intellitune", |run| matches!(run, Action::Goto { left, .. } if left == "App"));
    assert!(effects.contains(&format!("data-arg=\"Motion:intellitune:{help}\" disabled>Tuning Help<")));
    backend.calls.lock().unwrap().clear();
    for (arg, value) in [
        ("Motion:sway", "5"),
        ("Motion:sway:toggle", "0"),
        ("Motion:sway:max", "9"),
        ("Motion:motion_max_power", "10"),
        ("Motion:intellitune", "0"),
    ] {
        dispatch(&mut app, "set", arg, value).unwrap();
    }
    assert!(tune_patches(&backend).is_empty(), "{:?}", tune_patches(&backend));
    dispatch(&mut app, "action", &format!("Motion:intellitune:{help}"), "").unwrap();
    assert_eq!(app.ui.screen, Screen::Motion, "Tuning Help is not taken in stand-by");
    // The header's actions stay.
    let page = spec::motion_page(Registry::builtin(), 1.0);
    let hardware =
        page.header.iter().position(|b| matches!(b, Widget::Button { text, .. } if text == "Hardware setup")).unwrap();
    dispatch(&mut app, "action", &format!("Motion:@header:{hardware}"), "").unwrap();
    assert_eq!(app.ui.screen, Screen::Hardware);
    drop(app.take_threads());

    // A game without an effect: its switch, Max Telemetry and buttons are disabled with it and take nothing.
    let effects: Vec<&str> = Registry::builtin().motion_effects.iter().map(|e| e.id.as_str()).collect();
    let (game, column) = game_lacking(&effects);
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["motion_switch"][&column] = json!(true);
    let mut app = motion(&backend);
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    app.refresh_docs();
    app.ui.session = crate::ui::session::read(&*app.backend);
    let html: String = [EFFECTS, RACING, FLIGHT].iter().map(|index| group(&mut app, *index)).collect();
    let found = row(&html, &column).expect("the row is shown");
    assert!(tag(found, &format!("c-motion-{column}-toggle")).contains(" disabled"), "{found}");
    for button in found.split("data-act=\"action\"").skip(1) {
        assert!(button.split('>').next().unwrap().contains(" disabled"), "{column}: {button}");
    }
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", &format!("Motion:{column}:toggle"), "0").unwrap();
    dispatch(&mut app, "set", &format!("Motion:{column}"), "3").unwrap();
    assert!(tune_patches(&backend).is_empty());
    drop(app.take_threads());
}

#[test]
fn the_pitch_roll_and_yaw_smoothing_follow_the_game_features() {
    let (game, column) = game_lacking(&["pitch", "roll", "yaw"]);
    let name = format!("motion_smoothing_{column}");
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!(game);
    app.refresh_docs();
    app.ui.session = crate::ui::session::read(&*app.backend);
    let html = group(&mut app, SMOOTHING);
    assert!(row(&html, &name).expect("shown and disabled").contains(" disabled"), "{game} {name}");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", &format!("Motion:{name}"), "40").unwrap();
    assert!(tune_patches(&backend).is_empty());
    backend.docs.lock().unwrap()["settings"]["features"]["hide_effects_not_in_game"] = json!(true);
    app.refresh_docs();
    assert!(row(&group(&mut app, SMOOTHING), &name).is_none(), "{game} {name} hidden");
    drop(app.take_threads());
}

#[test]
fn the_number_boxes_keep_typed_values_within_their_range() {
    let backend = FakeBackend::new();
    let thanos = model_with(|m| m.manufacturer == "THANOS");
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] = json!(thanos.name);
    backend.docs.lock().unwrap()["status"]["devices"]["thanos"]["config"] = json!({"klm_mode": 1});
    let mut app = motion(&backend);
    for (arg, value, path, expected) in [
        ("Motion:pitch:max", "2500", vec!["max_telemetry_vars", "pitch"], 1800),
        ("Motion:pitch:max", "-2500", vec!["max_telemetry_vars", "pitch"], -1800),
        ("Motion:pitch:max", "12.7", vec!["max_telemetry_vars", "pitch"], 12),
        ("Motion:min_traction_loss_activation", "-5", vec!["min_traction_loss_activation"], 0),
        ("Motion:min_traction_loss_front_activation", "1801", vec!["min_traction_loss_front_activation"], 1800),
        ("Motion:thanos_spike_filter_vertical", "70000", vec!["thanos_spike_filter_vertical"], 64530),
        ("Motion:thanos_spike_filter_horizontal", "100", vec!["thanos_spike_filter_horizontal"], 255),
    ] {
        dispatch(&mut app, "set", arg, value).unwrap();
        assert_eq!(stored(&backend, &path), json!(expected), "{arg} {value}");
    }
    // A text that is no number leaves the box at what it showed.
    dispatch(&mut app, "set", "Motion:pitch:max", "many").unwrap();
    assert_eq!(stored(&backend, &["max_telemetry_vars", "pitch"]), json!(12));
    drop(app.take_threads());
}

#[test]
fn autotune_goes_on_behind_another_dialog_and_comes_back() {
    let backend = FakeBackend::new();
    let mut app = motion(&backend);
    app.ui.dialog = Some(Dialog::Wizard(WizardStep::Capture(30)));
    // A message opens over the capture: the wizard goes on behind it, the capture stays on.
    dialogs::open(&mut app, Dialog::Message { title: "Note".into(), text: "Hello".into() });
    assert!(matches!(app.ui.dialog, Some(Dialog::Message { .. })));
    assert!(wizard_open(&app), "the timer keeps running");
    set_step(&mut app, WizardStep::Capture(29));
    assert!(matches!(app.ui.dialog, Some(Dialog::Message { .. })), "the step moves on behind the message");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(!backend.patches("controls").contains(&json!({"telemetry": {"intellitune_capture": false}})));
    assert_eq!(app.ui.dialog, Some(Dialog::Wizard(WizardStep::Capture(29))), "the wizard comes back");
    // A failure while it is behind ends it without closing the other dialog.
    dialogs::open(&mut app, Dialog::Message { title: "Note".into(), text: "Again".into() });
    close_wizard(&mut app);
    assert!(matches!(app.ui.dialog, Some(Dialog::Message { .. })) && !wizard_open(&app));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

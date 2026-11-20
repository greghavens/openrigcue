//! Tests of Hardware → Motion Compensation: the rows and their rules, every stored value, calibration, the VR
//! status and set-up, the OpenVR settings dialog and the shortcut actions.

use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Theme};

const KEY: &str = "Motion Compensation";

fn screen(backend: &std::sync::Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Compensation, 0);
    drop(app.take_threads());
    app
}

fn set_model(backend: &FakeBackend, app: &mut MyApp, model: &str) {
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] = json!(model);
    app.refresh_docs();
}

fn posts(backend: &FakeBackend) -> Vec<String> {
    backend.calls.lock().unwrap().iter().filter(|(m, _, _)| *m == Method::Post).map(|(_, p, _)| p.clone()).collect()
}

fn status() -> Value {
    json!({
        "rig_pose": {"writing": true, "running": true, "rate_hz": 90.0, "path": "/run/pose", "published": {"enabled": true, "age_ms": 3.0, "values": {"sway": 0.1, "surge": 0, "heave": 0, "yaw": 0, "roll": 0, "pitch": 0}, "rates": null}},
        "consumer": {"kind": "openvr", "pid": 42, "live": true},
        "steamvr_driver": {"state": "loaded"},
        "imu": {"connection": "Connected", "port": "/dev/ttyUSB0", "angles": [1.0, 2.0, 3.0], "rates": [0.1, 0.2, 0.3], "age_ms": 5.0},
        "motion_sensor": {"pitch": {"value": 1.23456}, "roll": {"value": 2}, "yaw": {"value": 0}, "heave": {"value": 0}, "sway": {"value": 0}, "surge": {"value": 0}},
        "settings": {"cor_forward_mm": 100.0, "cor_down_mm": null, "cor_right_mm": 0.0, "srs_export": true}
    })
}

#[test]
fn the_groups_rows_and_tools_follow_the_design_in_every_theme() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    for theme in Theme::ALL {
        app.ui.theme = theme;
        let html = main(&app);
        for group in ["Sensor", "Settings", "VR status"] {
            assert!(html.contains(&format!(">{group}</option>")), "{group}");
        }
        assert!(html.contains("data-arg=\"Motion Compensation:sensor_type\""));
        assert!(html.contains("Motion Compensation:vr-setup:") && html.contains("Motion Compensation:calibrate-all:"));
        assert!(html.contains("Motion Compensation:@header:0"), "Actions → Shortcuts");
    }
    app.ui.theme = Theme::Redline;
    assert!(main(&app).contains("SENSOR"));
    dispatch(&mut app, "action", &format!("{KEY}:@header:0"), "").unwrap();
    let Some(crate::ui::dialogs::Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("shortcut editor") };
    assert!(editor.actions.iter().any(|(key, _)| key == "motion_compensation_all_calibrate"));
    assert!(editor.actions.iter().any(|(key, _)| key == "motion_compensation_heave_switch"));
    drop(app.take_threads());
}

#[test]
fn rows_follow_the_rig_and_the_sensor() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    // No six-DOF rig: no yaw, no heave, no rotation factors.
    let html = main(&app);
    assert!(!html.contains("Motion Compensation:yaw\"") && !html.contains("Motion Compensation:heave\""));
    assert!(!html.contains("Motion Compensation:pitch_factor\"") && html.contains("Motion Compensation:sway_factor\""));
    set_model(&backend, &mut app, "H6");
    let html = main(&app);
    assert!(html.contains("Motion Compensation:yaw\"") && html.contains("Motion Compensation:heave\""));
    assert!(html.contains("Motion Compensation:pitch_factor\""));
    set_model(&backend, &mut app, "H4");
    let html = main(&app);
    assert!(!html.contains("Motion Compensation:yaw\"") && html.contains("Motion Compensation:heave\""));
    // Surge rows only for a rig with a surge actuator.
    assert!(!html.contains("surge_horizontal_factor\""));
    let surge = crate::catalog::models().iter().find(|m| m.port_mapping.iter().any(|p| p == "Surge")).unwrap();
    set_model(&backend, &mut app, &surge.name);
    assert!(main(&app).contains("surge_horizontal_factor\""));
    // WITMotion: Calibrate under Options, the installation shown, the spacing hidden.
    backend.docs.lock().unwrap()["settings"]["compensation"]["sensor"] = json!("witmotion");
    app.refresh_docs();
    let html = main(&app);
    assert!(html.contains("data-arg=\"Motion Compensation:pitch:0\""));
    dispatch(&mut app, "section", "", "1").unwrap();
    let settings = main(&app);
    assert!(settings.contains("installation_mode\"") && !settings.contains("actuator_distance_left_right\""));
    // IntelliComp: no Calibrate, no installation, the spacing shown.
    backend.docs.lock().unwrap()["settings"]["compensation"]["sensor"] = json!("virtual");
    app.refresh_docs();
    let settings = main(&app);
    assert!(!settings.contains("installation_mode\"") && settings.contains("actuator_distance_left_right\""));
    dispatch(&mut app, "section", "", "0").unwrap();
    assert!(!main(&app).contains("data-arg=\"Motion Compensation:pitch:0\""));
    // A rig without the IntelliComp sensor: the sensor choice is locked.
    let locked = crate::catalog::models().iter().find(|m| !m.enable_intellicomp).unwrap();
    set_model(&backend, &mut app, &locked.name);
    let html = main(&app);
    let select = &html[html.find("id=\"c-motioncompensation-sensor_type\"").unwrap()..];
    assert!(select[..select.find('>').unwrap()].contains(" disabled"));
    drop(app.take_threads());
}

#[test]
fn every_row_stores_its_setting() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    set_model(&backend, &mut app, "H6");
    backend.calls.lock().unwrap().clear();
    for (control, value) in [
        ("pitch", "0"),
        ("roll_reverse", "1"),
        ("sway_factor", "40"),
        ("pitch_factor", "70"),
        ("surge_horizontal_factor", "30"),
        ("surge_horizontal_reverse", "1"),
        ("height", "900"),
        ("swap", "1"),
        ("installation_mode", "1"),
        ("actuator_distance_left_right", "600"),
        ("actuator_distance_front_back", "800"),
        ("vertical_actuators_stroke", "150"),
        ("surge_stroke", "100"),
        ("sensor_type", "0"),
    ] {
        dispatch(&mut app, "set", &format!("{KEY}:{control}"), value).unwrap();
    }
    let patches = backend.patches("settings");
    assert!(patches.iter().any(|p| p["compensation"]["pitch"]["enabled"] == json!(false)));
    assert!(patches.iter().any(|p| p["compensation"]["roll"]["reverse"] == json!(true)));
    let stored = |key: &str| backend.native_setting("MOTION_COMPENSATION", key).unwrap();
    for (key, expected) in [
        ("sway_factor", 40.0),
        ("pitch_factor", 70.0),
        ("surge_horizontal_factor", 30.0),
        ("height", 900.0),
        ("mm_distance_left_right", 600.0),
        ("mm_distance_front_back", 800.0),
        ("vertical_actuators_stroke", 150.0),
        ("surge_horizontal_stroke", 100.0),
    ] {
        assert_eq!(
            crate::backend::python_float(&crate::backend::native_json(&stored(key)).unwrap()).unwrap(),
            expected,
            "{key}"
        );
    }
    assert!(stored("surge_horizontal_reverse").is_truthy());
    assert!(stored("swap_roll_pitch").is_truthy());
    assert_eq!(stored("installation"), LegacyValue::Str("V".into()));
    assert_eq!(stored("type"), LegacyValue::Str("W".into()));
    drop(app.take_threads());
}

#[test]
fn calibration_asks_then_calibrates_one_or_all_axes() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    backend.docs.lock().unwrap()["settings"]["compensation"]["sensor"] = json!("witmotion");
    app.refresh_docs();
    dispatch(&mut app, "action", &format!("{KEY}:pitch:0"), "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("Calibrate Pitch"));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(posts(&backend), ["compensation/actions/pitch_calibrate"]);
    dispatch(&mut app, "action", &format!("{KEY}:calibrate-all:"), "").unwrap();
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(
        posts(&backend),
        [
            "compensation/actions/yaw_calibrate",
            "compensation/actions/roll_calibrate",
            "compensation/actions/pitch_calibrate"
        ]
    );
    // A failure is reported.
    backend.docs.lock().unwrap()["mc_calibration_failure"] = json!("roll");
    dispatch(&mut app, "action", &format!("{KEY}:roll:0"), "").unwrap();
    assert!(dispatch(&mut app, "dialog-ok", "", "").is_err());
    assert!(crate::app::tests::alert_text(&app).unwrap().contains("fixture calibration failure"));
    drop(app.take_threads());
}

#[test]
fn shortcut_actions_calibrate_and_toggle_axes() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    backend.calls.lock().unwrap().clear();
    backend.docs.lock().unwrap()["ui_actions"] =
        json!(["motion_compensation_all_calibrate", "motion_compensation_pitch_switch"]);
    let before = backend.native_setting("MOTION_COMPENSATION", "pitch").unwrap();
    let was_on = native_field_owned(&before, &["enabled"]).unwrap().is_truthy();
    rules(&mut app).unwrap();
    let calls = posts(&backend);
    for axis in ["pitch", "roll", "yaw"] {
        assert!(calls.contains(&format!("compensation/actions/{axis}_calibrate")), "{axis}");
    }
    assert!(backend.patches("settings").iter().any(|p| p["compensation"]["pitch"]["enabled"] == json!(!was_on)));
    drop(app.take_threads());
}

#[test]
fn the_vr_status_shows_live_readings_and_runs_the_component_setup() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["compensation_sequence"] = json!([status()]);
    backend.docs.lock().unwrap()["compensation/setup"] = json!({
        "steamvr_runtime": "/steam",
        "openxr": {"status": "Setup failed: no runtime", "registered": false, "action": "Install"},
        "steamvr": {"status": "Installed", "registered": true}
    });
    let mut app = screen(&backend);
    dispatch(&mut app, "section", "", &STATUS_GROUP.to_string()).unwrap();
    rules(&mut app).unwrap();
    let html = main(&app);
    assert!(html.contains("Writing; 90.0 Hz; /run/pose; enabled; sample 3.0 ms old"));
    assert!(html.contains("openvr; PID 42; Live") && html.contains("state: loaded"));
    assert!(html.contains("Connected /dev/ttyUSB0; X: 1.0000"));
    assert!(html.contains("Pitch 1.2346") && html.contains("Roll 2"));
    assert!(html.contains("Setup failed: no runtime") && html.contains(">Installed<"));
    // Install runs on its own thread; the button is busy meanwhile.
    dispatch(&mut app, "action", &format!("{KEY}:install.openxr:"), "").unwrap();
    assert!(main(&app).contains("aria-busy=\"true\""));
    assert_eq!(app.take_threads().len(), 1);
    finish_operation(&mut app, "openxr", Ok(json!({"openxr": {"status": "Installed", "registered": true}})));
    let html = main(&app);
    assert!(!html.contains(":install.openxr:") && html.contains(":uninstall.openxr:"));
    finish_operation(&mut app, "openxr", Err(crate::backend::Failure { status: 500, detail: "no permission".into() }));
    assert!(crate::app::tests::alert_text(&app).unwrap().contains("no permission"));
    dispatch(&mut app, "action", &format!("{KEY}:uninstall.steamvr_compensation:"), "").unwrap();
    assert_eq!(app.take_threads().len(), 1);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", &format!("{KEY}:recenter:"), "").unwrap();
    assert_eq!(posts(&backend), ["compensation/actions/recenter"]);
    dispatch(&mut app, "action", &format!("{KEY}:check:"), "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Get && p == "compensation/setup"));
    drop(app.take_threads());
}

#[test]
fn the_openvr_dialog_writes_the_centre_of_rotation_and_export() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["compensation_sequence"] = json!([status()]);
    let mut app = screen(&backend);
    dispatch(&mut app, "action", &format!("{KEY}:vr-setup:"), "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(
        html.contains("value=\"100\"") && html.contains("id=\"vr-cor_down_mm\"") && html.contains("SRS Pose Export")
    );
    let below = &html[html.find("id=\"vr-cor_down_mm\"").unwrap()..];
    assert!(below[..below.find('>').unwrap()].contains(" disabled"), "Below follows the seated height");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-field", "cor_forward_mm", "120.5").unwrap();
    dispatch(&mut app, "dialog-field", "seated", "0").unwrap();
    dispatch(&mut app, "dialog-field", "export", "0").unwrap();
    let vr = backend.patches("compensation/vr");
    assert_eq!(vr[0], json!({"cor_forward_mm": 120.5}));
    assert!(vr[1]["cor_down_mm"].is_number());
    assert_eq!(vr[2], json!({"srs_export": false}));
    // A distance that is not a finite number is refused.
    dispatch(&mut app, "dialog-field", "cor_right_mm", "abc").unwrap();
    assert_eq!(backend.patches("compensation/vr").len(), 3);
    assert_eq!(crate::app::tests::alert_text(&app).unwrap(), "Enter a finite distance in millimetres.");
    dispatch(&mut app, "dialog-field", "seated", "1").unwrap();
    assert_eq!(backend.patches("compensation/vr")[3], json!({"cor_down_mm": null}));
    drop(app.take_threads());
}

#[test]
fn every_compensation_text_has_an_entry() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["compensation_sequence"] = json!([status()]);
    backend.docs.lock().unwrap()["compensation/setup"] = json!({"openxr": {"status": "Invalid: x", "registered": false, "action": "Install"}, "steamvr": {"status": "Not installed", "registered": true}});
    let mut app = screen(&backend);
    set_model(&backend, &mut app, "H6");
    backend.translations.lock().unwrap().clear();
    for theme in Theme::ALL {
        app.ui.theme = theme;
        for section in 0..=STATUS_GROUP {
            app.ui.section = section;
            main(&app);
        }
    }
    refresh_live(&mut app);
    main(&app);
    app.ui.devices.compensation.status = None;
    app.ui.devices.compensation.setup = None;
    main(&app);
    app.ui.devices.compensation.status = Some(Err("x".into()));
    app.ui.devices.compensation.setup = Some(Err("x".into()));
    main(&app);
    status_lines(&app, &json!({"rig_pose": {"error": "x"}, "imu": {}, "consumer": {"pid": 1}}));
    for dialog in [
        Dialog::Calibrate { axis: "all".into() },
        Dialog::Calibrate { axis: "yaw".into() },
        Dialog::Vr { settings: status()["settings"].clone() },
        Dialog::Vr { settings: json!({"cor_down_mm": 5.0, "srs_export": false}) },
    ] {
        dialog_render(&app, &dialog);
    }
    let looked_up: BTreeSet<String> = backend.translations.lock().unwrap().iter().cloned().collect();
    let missing = crate::app::tests::texts_without_entry(&looked_up, &["x", "Unknown", "no runtime"]);
    assert_eq!(missing, Vec::<String>::new());
    drop(app.take_threads());
}

// ---- The rows, their rules and their values ---------------------------------------------------------------

/// A PT-ACTUATOR rig with a Surge port (not DOF REALITY, IntelliComp available).
const SURGE_RIG: &str = "5 Actuators (2 Front + 2 Rear + Surge) CANBus";

/// The rows of a rendered group, in order.
fn rows_of(html: &str) -> Vec<String> {
    html.split("data-control=\"").skip(1).map(|rest| rest[..rest.find('"').unwrap()].to_owned()).collect()
}

/// The opening tag of the element with the id `id`.
fn tag<'a>(html: &'a str, id: &str) -> &'a str {
    let at = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element {id}"));
    let start = html[..at].rfind('<').unwrap();
    &html[start..=at + html[at..].find('>').unwrap()]
}

/// Whether the switch of a row is on.
fn is_on(html: &str, control: &str) -> bool {
    tag(html, &format!("c-motioncompensation-{control}")).contains(" checked")
}

/// The `value` of a row's input.
fn value_of(html: &str, control: &str) -> String {
    let tag = tag(html, &format!("c-motioncompensation-{control}"));
    let rest = &tag[tag.find("value=\"").unwrap() + 7..];
    rest[..rest.find('"').unwrap()].to_owned()
}

/// The chosen option of a row's select.
fn selected(html: &str, control: &str) -> String {
    let start = html.find(&format!("id=\"c-motioncompensation-{control}\"")).unwrap();
    let body = &html[start..start + html[start..].find("</select>").unwrap()];
    let chosen = body.find(" selected>").unwrap();
    let value = body[..chosen].rfind("value=\"").unwrap() + 7;
    body[value..chosen - 1].to_owned()
}

/// The group `section` as shown.
fn group(app: &mut MyApp, section: usize) -> String {
    dispatch(app, "section", "", &section.to_string()).unwrap();
    main(app)
}

fn set_sensor(backend: &FakeBackend, app: &mut MyApp, sensor: &str) {
    backend.docs.lock().unwrap()["settings"]["compensation"]["sensor"] = json!(sensor);
    app.refresh_docs();
}

/// The `compensation` parts of the settings writes.
fn compensation_patches(backend: &FakeBackend) -> Vec<Value> {
    backend.patches("settings").into_iter().filter(|p| !p["compensation"].is_null()).collect()
}

/// The stored Motion Compensation settings of the rows-shown tests.
fn stored_settings(backend: &FakeBackend, app: &mut MyApp) {
    let axis = |enabled: bool, reverse: bool| json!({"offset": 0.0, "enabled": enabled, "reverse": reverse});
    {
        let mut docs = backend.docs.lock().unwrap();
        let compensation = &mut docs["settings"]["compensation"];
        for (key, value) in [
            ("yaw", axis(false, false)),
            ("roll", axis(true, false)),
            ("pitch", axis(true, true)),
            ("heave", axis(false, false)),
            ("surge", axis(true, true)),
            ("sway", axis(true, false)),
            ("height", json!(1000.0)),
            ("swap_roll_pitch", json!(true)),
            ("installation", json!("V")),
            ("sway_factor", json!(80.0)),
            ("surge_factor", json!(70.0)),
            ("surge_horizontal_factor", json!(10.0)),
            ("surge_horizontal_stroke", json!(100.0)),
            ("surge_horizontal_reverse", json!(true)),
            ("sensor", json!("witmotion")),
            ("left_right_distance_mm", json!(558.0)),
            ("front_back_distance_mm", json!(1270.0)),
            ("actuator_vertical_stroke", json!(150.0)),
            ("roll_factor", json!(90.0)),
            ("pitch_factor", json!(60.0)),
            ("yaw_factor", json!(50.0)),
        ] {
            compensation[key] = value;
        }
    }
    app.refresh_docs();
}

#[test]
fn each_group_lists_its_rows_in_order_for_the_rig_and_the_sensor() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    let strings = |rows: &[&str]| rows.iter().map(|row| (*row).to_owned()).collect::<Vec<_>>();
    let six_dof = strings(&[
        "sensor_type",
        "pitch",
        "pitch_reverse",
        "pitch_factor",
        "roll",
        "roll_reverse",
        "roll_factor",
        "yaw",
        "yaw_reverse",
        "yaw_factor",
        "heave",
        "heave_reverse",
        "sway",
        "sway_reverse",
        "sway_factor",
        "surge",
        "surge_reverse",
        "surge_factor",
    ]);
    // H6 and P6: every rotation has a factor, heave has none.
    for model in ["H6", "P6"] {
        set_model(&backend, &mut app, model);
        set_sensor(&backend, &mut app, "witmotion");
        assert_eq!(rows_of(&group(&mut app, 0)), six_dof, "{model}");
        assert_eq!(
            rows_of(&group(&mut app, 1)),
            strings(&["height", "swap", "installation_mode", "vertical_actuators_stroke"])
        );
    }
    // A three-DOF rig: only sway and surge have a factor; no yaw, no heave.
    set_model(&backend, &mut app, "H3");
    let three_dof = strings(&[
        "sensor_type",
        "pitch",
        "pitch_reverse",
        "roll",
        "roll_reverse",
        "sway",
        "sway_reverse",
        "sway_factor",
        "surge",
        "surge_reverse",
        "surge_factor",
    ]);
    assert_eq!(rows_of(&group(&mut app, 0)), three_dof);
    // A rig with a surge actuator: the horizontal factor and direction, and the surge stroke.
    set_model(&backend, &mut app, SURGE_RIG);
    let mut surge = three_dof.clone();
    surge.extend(strings(&["surge_horizontal_factor", "surge_horizontal_reverse"]));
    assert_eq!(rows_of(&group(&mut app, 0)), surge);
    assert_eq!(
        rows_of(&group(&mut app, 1)),
        strings(&["height", "swap", "installation_mode", "vertical_actuators_stroke", "surge_stroke"])
    );
    set_sensor(&backend, &mut app, "virtual");
    assert_eq!(
        rows_of(&group(&mut app, 1)),
        strings(&[
            "height",
            "swap",
            "actuator_distance_left_right",
            "actuator_distance_front_back",
            "vertical_actuators_stroke",
            "surge_stroke"
        ])
    );
    drop(app.take_threads());
}

#[test]
fn calibrate_is_offered_for_pitch_roll_and_yaw_only_with_the_witmotion_sensor() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    set_model(&backend, &mut app, "H6");
    set_sensor(&backend, &mut app, "witmotion");
    assert_eq!(backend.native_setting("MOTION_COMPENSATION", "type").unwrap(), LegacyValue::Str("W".into()));
    let html = group(&mut app, 0);
    for (axis, offered) in
        [("pitch", true), ("roll", true), ("yaw", true), ("heave", false), ("sway", false), ("surge", false)]
    {
        assert_eq!(html.contains(&format!("data-arg=\"{KEY}:{axis}:0\"")), offered, "{axis}");
    }
    set_sensor(&backend, &mut app, "virtual");
    assert_eq!(backend.native_setting("MOTION_COMPENSATION", "type").unwrap(), LegacyValue::Str("V".into()));
    let html = group(&mut app, 0);
    for axis in ["pitch", "roll", "yaw", "heave", "sway", "surge"] {
        assert!(!html.contains(&format!("data-arg=\"{KEY}:{axis}:0\"")), "{axis}");
    }
    drop(app.take_threads());
}

#[test]
fn the_sensor_choice_is_stored_and_decides_calibration_installation_and_spacing() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    // H3 is a DOF REALITY rig.
    set_model(&backend, &mut app, "H3");
    set_sensor(&backend, &mut app, "witmotion");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", &format!("{KEY}:sensor_type"), "1").unwrap();
    assert_eq!(compensation_patches(&backend), [json!({"compensation": {"sensor": "virtual"}})]);
    // IntelliComp: no Calibrate, no installation; DOF REALITY: no spacing and no vertical stroke.
    let sensor = group(&mut app, 0);
    assert!(!sensor.contains(":pitch:0\"") && !sensor.contains(":roll:0\"") && !sensor.contains(":yaw:0\""));
    assert_eq!(rows_of(&group(&mut app, 1)), ["height", "swap"]);
    dispatch(&mut app, "set", &format!("{KEY}:sensor_type"), "0").unwrap();
    assert_eq!(compensation_patches(&backend).last().unwrap(), &json!({"compensation": {"sensor": "witmotion"}}));
    // WITMotion: Calibrate, the installation; still no spacing.
    let sensor = group(&mut app, 0);
    assert!(sensor.contains(":pitch:0\"") && sensor.contains(":roll:0\""));
    assert_eq!(rows_of(&group(&mut app, 1)), ["height", "swap", "installation_mode", "vertical_actuators_stroke"]);
    // The rig the service has now decides: another manufacturer shows the spacing and the stroke with IntelliComp.
    dispatch(&mut app, "set", &format!("{KEY}:sensor_type"), "1").unwrap();
    set_model(&backend, &mut app, SURGE_RIG);
    let settings = rows_of(&group(&mut app, 1));
    for row in ["actuator_distance_left_right", "actuator_distance_front_back", "vertical_actuators_stroke"] {
        assert!(settings.iter().any(|shown| shown == row), "{row}");
    }
    assert!(!settings.iter().any(|shown| shown == "installation_mode"));
    drop(app.take_threads());
}

#[test]
fn yaw_shows_only_on_h6_and_p6_and_heave_also_on_h4_and_p4() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    for (model, yaw, heave) in [
        ("H6", true, true),
        ("P6", true, true),
        ("H4", false, true),
        ("P4", false, true),
        ("H3", false, false),
        // A Yaw VR rig: the yaw row follows the six-DOF rule too.
        ("Yaw3", false, false),
        // A model the catalog does not know: no yaw, no heave, and the per-pass rules still run.
        ("No Such Rig", false, false),
    ] {
        set_model(&backend, &mut app, model);
        let rows = rows_of(&group(&mut app, 0));
        assert_eq!(rows.iter().any(|row| row == "yaw"), yaw, "{model}: yaw");
        assert_eq!(rows.iter().any(|row| row == "heave"), heave, "{model}: heave");
        rules(&mut app).unwrap();
    }
    // A stored model that is not a name.
    backend.docs.lock().unwrap()["settings"]["motion"]["model"] = json!([]);
    app.refresh_docs();
    let rows = rows_of(&group(&mut app, 0));
    assert!(!rows.iter().any(|row| row == "yaw" || row == "heave"));
    rules(&mut app).unwrap();
    drop(app.take_threads());
}

#[test]
fn every_value_is_written_with_its_stored_kind() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    set_model(&backend, &mut app, "H6");
    backend.calls.lock().unwrap().clear();
    for (control, value, expected) in [
        ("roll", "0", json!({"roll": {"enabled": false}})),
        ("roll", "1", json!({"roll": {"enabled": true}})),
        ("roll_reverse", "1", json!({"roll": {"reverse": true}})),
        ("pitch_factor", "42", json!({"pitch_factor": 42})),
        ("sway_factor", "7", json!({"sway_factor": 7})),
        ("surge_horizontal_factor", "30", json!({"surge_horizontal_factor": 30})),
        ("surge_horizontal_reverse", "1", json!({"surge_horizontal_reverse": true})),
        ("swap", "1", json!({"swap_roll_pitch": "yes"})),
        ("swap", "0", json!({"swap_roll_pitch": "no"})),
        ("installation_mode", "1", json!({"installation": "V"})),
        ("installation_mode", "0", json!({"installation": "H"})),
        ("height", " 12 ", json!({"height": 12})),
        ("actuator_distance_left_right", " 12 ", json!({"left_right_distance_mm": 12})),
        ("actuator_distance_front_back", " 12 ", json!({"front_back_distance_mm": 12})),
        ("vertical_actuators_stroke", " 12 ", json!({"actuator_vertical_stroke": 12})),
        // A fraction is dropped, not rounded.
        ("height", "12.5", json!({"height": 12})),
        ("height", "12.9", json!({"height": 12})),
        ("pitch_factor", "41.7", json!({"pitch_factor": 41})),
        // A number box keeps the value within its limits first.
        ("surge_stroke", "-3.9", json!({"surge_horizontal_stroke": 1})),
        ("pitch_factor", "42", json!({"pitch_factor": 42})),
        ("surge_stroke", " 12 ", json!({"surge_horizontal_stroke": 12})),
    ] {
        dispatch(&mut app, "set", &format!("{KEY}:{control}"), value).unwrap();
        assert_eq!(
            compensation_patches(&backend).last().unwrap(),
            &json!({ "compensation": expected }),
            "{control} = {value:?}"
        );
    }
    // The row shows what was stored.
    let sensor = group(&mut app, 0);
    assert_eq!(value_of(&sensor, "pitch_factor"), "42");
    assert!(is_on(&sensor, "roll") && is_on(&sensor, "roll_reverse"));
    // The number rows' ranges.
    set_model(&backend, &mut app, SURGE_RIG);
    set_sensor(&backend, &mut app, "virtual");
    let settings = group(&mut app, 1);
    for (row, max) in [
        ("height", "1200"),
        ("actuator_distance_left_right", "3000"),
        ("actuator_distance_front_back", "3000"),
        ("vertical_actuators_stroke", "250"),
        ("surge_stroke", "250"),
    ] {
        let input = tag(&settings, &format!("c-motioncompensation-{row}"));
        assert!(input.contains("min=\"1\"") && input.contains(&format!("max=\"{max}\"")), "{row}: {input}");
        assert_eq!(value_of(&settings, row), "12", "{row}");
    }
    drop(app.take_threads());
}

#[test]
fn opening_the_screen_writes_nothing_and_the_first_change_is_stored() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["compensation_sequence"] = json!([status()]);
    let mut app = loaded(backend.clone());
    set_model(&backend, &mut app, SURGE_RIG);
    stored_settings(&backend, &mut app);
    backend.calls.lock().unwrap().clear();
    navigate(&mut app, Screen::Compensation, 0);
    for section in 0..=STATUS_GROUP {
        group(&mut app, section);
    }
    rules(&mut app).unwrap();
    assert_eq!(compensation_patches(&backend), Vec::<Value>::new());
    // The very first change of the swap, the installation and the sensor is stored.
    for (control, value, expected) in [
        ("swap", "0", json!({"swap_roll_pitch": "no"})),
        ("installation_mode", "1", json!({"installation": "V"})),
        ("sensor_type", "1", json!({"sensor": "virtual"})),
    ] {
        dispatch(&mut app, "set", &format!("{KEY}:{control}"), value).unwrap();
        assert_eq!(compensation_patches(&backend).last().unwrap(), &json!({ "compensation": expected }), "{control}");
    }
    assert_eq!(compensation_patches(&backend).len(), 3);
    drop(app.take_threads());
}

#[test]
fn the_rows_show_the_stored_values() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    set_model(&backend, &mut app, "H6");
    stored_settings(&backend, &mut app);
    let sensor = group(&mut app, 0);
    assert_eq!(selected(&sensor, "sensor_type"), "0", "WITMotion");
    assert!(is_on(&sensor, "pitch") && is_on(&sensor, "pitch_reverse"));
    assert!(!is_on(&sensor, "yaw") && !is_on(&sensor, "yaw_reverse"));
    assert!(is_on(&sensor, "surge_reverse"));
    for (row, value) in [
        ("sway_factor", "80"),
        ("surge_factor", "70"),
        ("pitch_factor", "60"),
        ("roll_factor", "90"),
        ("yaw_factor", "50"),
    ] {
        assert_eq!(value_of(&sensor, row), value, "{row}");
    }
    let settings = group(&mut app, 1);
    assert_eq!(value_of(&settings, "height"), "1000");
    assert!(is_on(&settings, "swap"));
    assert_eq!(selected(&settings, "installation_mode"), "1", "Vertical");
    assert_eq!(value_of(&settings, "vertical_actuators_stroke"), "150");
    // The surge rows of a rig with a surge actuator; the spacing with IntelliComp.
    set_model(&backend, &mut app, SURGE_RIG);
    set_sensor(&backend, &mut app, "virtual");
    let sensor = group(&mut app, 0);
    assert_eq!(selected(&sensor, "sensor_type"), "1", "IntelliComp");
    assert_eq!(value_of(&sensor, "surge_horizontal_factor"), "10");
    assert!(is_on(&sensor, "surge_horizontal_reverse"));
    let settings = group(&mut app, 1);
    assert_eq!(value_of(&settings, "actuator_distance_left_right"), "558");
    assert_eq!(value_of(&settings, "actuator_distance_front_back"), "1270");
    assert_eq!(value_of(&settings, "surge_stroke"), "100");
    // A stored text "yes" / "no" of the swap reads back as on / off.
    dispatch(&mut app, "set", &format!("{KEY}:swap"), "0").unwrap();
    assert!(!is_on(&group(&mut app, 1), "swap"));
    dispatch(&mut app, "set", &format!("{KEY}:swap"), "1").unwrap();
    assert!(is_on(&group(&mut app, 1), "swap"));
    // The current value is shown: a change in the service shows on the next render.
    backend.docs.lock().unwrap()["settings"]["compensation"]["pitch"]["enabled"] = json!(false);
    app.refresh_docs();
    assert!(!is_on(&group(&mut app, 0), "pitch"));
    drop(app.take_threads());
}

#[test]
fn broken_stored_values_still_render_and_write_nothing() {
    for broken in [json!(null), json!({"pitch": {"offset": 0.0, "enabled": false, "reverse": "on"}})] {
        let backend = FakeBackend::new();
        let mut app = screen(&backend);
        set_model(&backend, &mut app, "H6");
        backend.docs.lock().unwrap()["settings"]["compensation"] = broken.clone();
        app.refresh_docs();
        backend.calls.lock().unwrap().clear();
        for section in 0..=STATUS_GROUP {
            group(&mut app, section);
        }
        start(&mut app).unwrap();
        rules(&mut app).unwrap();
        assert_eq!(compensation_patches(&backend), Vec::<Value>::new(), "{broken}");
        assert_eq!(backend.docs.lock().unwrap()["settings"]["compensation"], broken);
        drop(app.take_threads());
    }
}

#[test]
fn invalid_numbers_and_choices_are_refused_without_a_write() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    set_model(&backend, &mut app, "H6");
    backend.calls.lock().unwrap().clear();
    for (control, value) in [
        ("pitch_factor", "NaN"),
        ("surge_horizontal_factor", "NaN"),
        ("sway_factor", "abc"),
        ("height", "nan"),
        ("actuator_distance_front_back", "NaN"),
        ("installation_mode", ""),
        ("installation_mode", "2"),
        ("sensor_type", "Vertical"),
    ] {
        assert!(dispatch(&mut app, "set", &format!("{KEY}:{control}"), value).is_err(), "{control} = {value:?}");
    }
    assert_eq!(compensation_patches(&backend), Vec::<Value>::new());
    drop(app.take_threads());
}

/// The five Settings number boxes keep a typed value within their limits before it is stored (a fraction
/// dropped); text that is no number stores the value the box shows.
#[test]
fn the_settings_number_boxes_keep_typed_values_within_their_limits() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    set_model(&backend, &mut app, SURGE_RIG);
    let stored = |backend: &FakeBackend, key: &str| {
        crate::backend::native_int(&backend.native_setting("MOTION_COMPENSATION", key).unwrap()).unwrap()
    };
    for (control, key, max) in [
        ("height", "height", 1200),
        ("actuator_distance_left_right", "mm_distance_left_right", 3000),
        ("actuator_distance_front_back", "mm_distance_front_back", 3000),
        ("vertical_actuators_stroke", "vertical_actuators_stroke", 250),
        ("surge_stroke", "surge_horizontal_stroke", 250),
    ] {
        for (typed, expected) in
            [("99999", max), ("0", 1), ("-7", 1), ("0.5", 1), ("inf", max), ("-inf", 1), ("17.8", 17)]
        {
            dispatch(&mut app, "set", &format!("{KEY}:{control}"), typed).unwrap();
            assert_eq!(stored(&backend, key), expected, "{control} = {typed:?}");
        }
        for unreadable in ["", "abc", "-"] {
            backend.calls.lock().unwrap().clear();
            dispatch(&mut app, "set", &format!("{KEY}:{control}"), unreadable).unwrap();
            assert_eq!(stored(&backend, key), 17, "{control} = {unreadable:?} keeps the shown value");
            assert_eq!(compensation_patches(&backend).len(), 1, "{control}: the shown value is stored again");
        }
    }
    drop(app.take_threads());
}

/// At start-up (the Windows rules apply) each Settings number box takes its stored setting again, in the order
/// height, surge stroke, vertical stroke, left-right and front-back spacing, so a stored value outside the box's
/// limits is stored within them.
#[test]
fn the_start_puts_the_stored_settings_within_the_boxes_limits() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        let compensation = &mut docs["settings"]["compensation"];
        compensation["height"] = json!(5000);
        compensation["surge_horizontal_stroke"] = json!(0);
        compensation["actuator_vertical_stroke"] = json!(120.9);
        compensation["left_right_distance_mm"] = json!(-40);
        compensation["front_back_distance_mm"] = json!(800);
    }
    let mut app = loaded(backend.clone());
    let stored = |key: &str| crate::backend::native_int(&backend.native_setting("MOTION_COMPENSATION", key).unwrap());
    assert_eq!(stored("height"), Ok(1200));
    assert_eq!(stored("surge_horizontal_stroke"), Ok(1));
    assert_eq!(stored("vertical_actuators_stroke"), Ok(120));
    assert_eq!(stored("mm_distance_left_right"), Ok(1));
    assert_eq!(stored("mm_distance_front_back"), Ok(800));
    let written: Vec<String> = compensation_patches(&backend)
        .iter()
        .filter_map(|patch| patch["compensation"].as_object().and_then(|keys| keys.keys().next().cloned()))
        .collect();
    assert_eq!(
        written[..5],
        [
            "height",
            "surge_horizontal_stroke",
            "actuator_vertical_stroke",
            "left_right_distance_mm",
            "front_back_distance_mm"
        ]
    );
    drop(app.take_threads());
}

/// After the number boxes, start-up puts each factor into its slider again (surge, horizontal surge, sway, then
/// on a six-DOF platform yaw, roll and pitch): each is stored as a whole number within 0…100. Without a six-DOF
/// platform the yaw, roll and pitch factors are left as they are.
#[test]
fn the_start_puts_the_factors_into_their_sliders_after_the_boxes() {
    for (model, six_dof) in [("None", false), ("H6", true)] {
        let backend = FakeBackend::new();
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["settings"]["motion"]["model"] = json!(model);
            let compensation = &mut docs["settings"]["compensation"];
            compensation["height"] = json!(5000);
            compensation["surge_factor"] = json!(140);
            compensation["surge_horizontal_factor"] = json!(-3);
            compensation["sway_factor"] = json!(12.6);
            compensation["yaw_factor"] = json!(300);
            compensation["roll_factor"] = json!(-1);
            compensation["pitch_factor"] = json!(55.5);
        }
        let mut app = loaded(backend.clone());
        let stored = |key: &str| backend.docs.lock().unwrap()["settings"]["compensation"][key].clone();
        assert_eq!(stored("surge_factor"), json!(100), "{model}");
        assert_eq!(stored("surge_horizontal_factor"), json!(0), "{model}");
        assert_eq!(stored("sway_factor"), json!(12), "{model}");
        if six_dof {
            assert_eq!(
                (stored("yaw_factor"), stored("roll_factor"), stored("pitch_factor")),
                (json!(100), json!(0), json!(55))
            );
        } else {
            assert_eq!(
                (stored("yaw_factor"), stored("roll_factor"), stored("pitch_factor")),
                (json!(300), json!(-1), json!(55.5))
            );
        }
        let written: Vec<String> = compensation_patches(&backend)
            .iter()
            .filter_map(|patch| patch["compensation"].as_object().and_then(|keys| keys.keys().next().cloned()))
            .collect();
        let at = |key: &str| written.iter().position(|k| k == key).unwrap_or_else(|| panic!("{key} in {written:?}"));
        assert!(at("height") < at("surge_factor"), "{written:?}");
        assert!(
            at("surge_factor") < at("surge_horizontal_factor") && at("surge_horizontal_factor") < at("sway_factor")
        );
        if six_dof {
            assert!(at("sway_factor") < at("yaw_factor") && at("yaw_factor") < at("roll_factor"));
            assert!(at("roll_factor") < at("pitch_factor"), "{written:?}");
        }
        drop(app.take_threads());
    }
}

/// A stored Settings value that is no whole number is left as it is; the other boxes are still corrected and the
/// start goes on.
#[test]
fn a_stored_setting_that_is_no_whole_number_is_left_at_the_start() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    backend.docs.lock().unwrap()["settings"]["compensation"]["height"] = json!("tall");
    backend.docs.lock().unwrap()["settings"]["compensation"]["front_back_distance_mm"] = json!(9000);
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    start(&mut app).unwrap();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["compensation"]["height"], json!("tall"));
    assert!(!compensation_patches(&backend).iter().any(|patch| patch["compensation"].get("height").is_some()));
    assert_eq!(backend.docs.lock().unwrap()["settings"]["compensation"]["front_back_distance_mm"], json!(3000));
    drop(app.take_threads());
}

#[test]
fn a_missing_settings_parent_refuses_the_write() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    // A rig that offers the IntelliComp sensor, so the sensor choice is written.
    set_model(&backend, &mut app, "H3");
    // The axis is missing: its switches are not written.
    backend.docs.lock().unwrap()["settings"]["compensation"]["pitch"] = Value::Null;
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    for control in ["pitch", "pitch_reverse"] {
        let error = dispatch(&mut app, "set", &format!("{KEY}:{control}"), "1").unwrap_err();
        assert!(error.starts_with("TypeError"), "{control}: {error}");
    }
    assert_eq!(backend.docs.lock().unwrap()["settings"]["compensation"]["pitch"], Value::Null);
    // The whole section is missing: nothing is written.
    backend.docs.lock().unwrap()["settings"]["compensation"] = Value::Null;
    app.refresh_docs();
    for (control, value) in [("sensor_type", "1"), ("pitch_factor", "33"), ("swap", "1"), ("height", "900")] {
        assert!(dispatch(&mut app, "set", &format!("{KEY}:{control}"), value).is_err(), "{control}");
    }
    assert_eq!(compensation_patches(&backend), Vec::<Value>::new());
    assert_eq!(backend.docs.lock().unwrap()["settings"]["compensation"], Value::Null);
    drop(app.take_threads());
}

// ---- The shortcut actions and the main loop ---------------------------------------------------------------

fn queue(backend: &FakeBackend, actions: &[&str]) {
    backend.docs.lock().unwrap()["ui_actions"] = json!(actions);
}

#[test]
fn the_shortcut_calibrations_run_pitch_roll_yaw_and_the_axis_switches_toggle() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    backend.calls.lock().unwrap().clear();
    queue(&backend, &["motion_compensation_all_calibrate", "motion_compensation_roll_switch"]);
    rules(&mut app).unwrap();
    assert_eq!(
        posts(&backend),
        [
            "compensation/actions/pitch_calibrate",
            "compensation/actions/roll_calibrate",
            "compensation/actions/yaw_calibrate"
        ]
    );
    assert_eq!(compensation_patches(&backend), [json!({"compensation": {"roll": {"enabled": false}}})]);
    assert!(!is_on(&group(&mut app, 0), "roll"), "the switch follows");
    // One axis's calibration; a failure is reported and the later calibrations of the pass are not made.
    for (pending, failing, expected) in [
        ("motion_compensation_yaw_calibrate", None, vec!["compensation/actions/yaw_calibrate"]),
        (
            "motion_compensation_all_calibrate",
            Some("roll"),
            vec!["compensation/actions/pitch_calibrate", "compensation/actions/roll_calibrate"],
        ),
    ] {
        backend.calls.lock().unwrap().clear();
        backend.docs.lock().unwrap()["mc_calibration_failure"] = json!(failing);
        queue(&backend, &[pending]);
        rules(&mut app).unwrap();
        assert_eq!(posts(&backend), expected, "{pending}");
    }
    assert!(crate::app::tests::alert_text(&app).unwrap().contains("fixture calibration failure"));
    drop(app.take_threads());
}

#[test]
fn the_axis_switch_shortcut_tests_the_stored_flag_for_truth() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    let pitch_switch = |app: &mut MyApp| {
        queue(&backend, &["motion_compensation_pitch_switch"]);
        rules(app)
    };
    // Off becomes on.
    app.set_setting(&["compensation", "pitch", "enabled"], json!(false));
    pitch_switch(&mut app).unwrap();
    assert_eq!(backend.docs.lock().unwrap()["settings"]["compensation"]["pitch"]["enabled"], json!(true));
    // A stored text "False" is not empty and counts as on: the toggle writes off, then on.
    app.set_setting(&["compensation", "pitch", "enabled"], json!("False"));
    backend.calls.lock().unwrap().clear();
    pitch_switch(&mut app).unwrap();
    assert_eq!(compensation_patches(&backend), [json!({"compensation": {"pitch": {"enabled": false}}})]);
    pitch_switch(&mut app).unwrap();
    assert_eq!(app.setting(&["compensation", "pitch", "enabled"]), &json!(true));
    // A missing flag stops the pass and is not repaired.
    backend.docs.lock().unwrap()["settings"]["compensation"]["pitch"].as_object_mut().unwrap().remove("enabled");
    backend.calls.lock().unwrap().clear();
    assert!(pitch_switch(&mut app).unwrap_err().contains("KeyError"));
    assert_eq!(compensation_patches(&backend), Vec::<Value>::new());
    assert!(backend.docs.lock().unwrap()["settings"]["compensation"]["pitch"].get("enabled").is_none());
    drop(app.take_threads());
}

#[test]
fn the_main_loop_pass_runs_the_queued_calibration_then_clears_the_queue() {
    let backend = FakeBackend::new();
    let mut app = screen(&backend);
    backend.calls.lock().unwrap().clear();
    queue(&backend, &["motion_compensation_pitch_calibrate"]);
    app.main_loop_pass();
    assert_eq!(app.ml.fatal_error, None);
    let calibrations: Vec<String> =
        posts(&backend).into_iter().filter(|path| path.starts_with("compensation/actions/")).collect();
    assert_eq!(calibrations, ["compensation/actions/pitch_calibrate"]);
    assert_eq!(backend.docs.lock().unwrap()["ui_actions"], json!([]));
    drop(app.take_threads());
}

#[test]
fn the_axis_values_read_each_axis_from_the_current_reading() {
    let backend = FakeBackend::new();
    // Each read answers the next reading: the status first, then pitch, then roll and the others.
    backend.docs.lock().unwrap()["compensation_sequence"] = json!([
        {"motion_sensor": {"pitch": {"value": 5}, "roll": {"value": 5}}},
        {"motion_sensor": {"pitch": {"value": 1}, "roll": {"value": 2}}},
        {"motion_sensor": {"pitch": {"value": 3}, "roll": {"value": 9}, "yaw": {"value": true},
            "sway": {"value": -0.000_01}, "surge": {"value": 1.234_56}}}
    ]);
    let mut app = screen(&backend);
    assert!(group(&mut app, STATUS_GROUP).contains("Pitch 1 · Roll 9 · Yaw 1 · Heave 0 · Sway -0.0 · Surge 1.2346"));
    for (value, text) in [
        (LegacyValue::Int(0), "0"),
        (LegacyValue::Float(0.0), "0.0"),
        (LegacyValue::Float(12.000_04), "12.0"),
        (LegacyValue::Float(-7.654_32), "-7.6543"),
        (LegacyValue::Float(1.234_56), "1.2346"),
        (LegacyValue::Float(-0.000_01), "-0.0"),
        (LegacyValue::Bool(true), "1"),
        (LegacyValue::Bool(false), "0"),
        (LegacyValue::None, "0"),
    ] {
        assert_eq!(sensor_text(&value), text, "{value:?}");
    }
    drop(app.take_threads());
}

// ---- The VR status, the component set-up and the OpenVR setup dialog --------------------------------------

/// The fake service with the `compensation` calls failing or the component set-up answering as a test sets.
struct VrService {
    inner: std::sync::Arc<FakeBackend>,
    /// Every `compensation…` call fails with "VR service unavailable: <path>".
    fail: std::sync::Mutex<bool>,
    /// The answer of `compensation/setup` and of its install / uninstall calls, when set.
    setup: std::sync::Mutex<Option<Result<Value, Failure>>>,
}

impl Backend for VrService {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if path == "compensation" || path.starts_with("compensation/") {
            if *self.fail.lock().unwrap() {
                self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
                return Err(Failure { status: 503, detail: format!("VR service unavailable: {path}") });
            }
            if path == "compensation/setup" || path.starts_with("compensation/setup/") {
                if let Some(answer) = self.setup.lock().unwrap().clone() {
                    self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
                    return answer;
                }
            }
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        self.inner.native_setting(section, key)
    }
    fn ui_actions(&self) -> Vec<String> {
        self.inner.ui_actions()
    }
    fn clear_ui_actions(&self) {
        self.inner.clear_ui_actions()
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A VR status with every reading.
fn vr_status() -> Value {
    json!({
        "rig_pose": {"running": true, "writing": true, "rate_hz": 200.0, "path": "/tmp/rig-pose", "error": null,
            "published": {
                "values": {"pitch": 0.25, "roll": -0.5, "yaw": 0.0, "heave": 0.1, "surge": 0.0, "sway": 0.0},
                "rates": {"pitch": 1.0, "roll": 2.0, "yaw": 3.0, "heave": 0.0, "surge": 0.0, "sway": 0.0}}},
        "imu": {"connection": "Connected", "port": "/dev/fake-imu", "angles": [1.0, 2.0, 3.0]},
        "consumer": {"kind": "SteamVR driver", "pid": 42, "live": true},
        "steamvr_driver": {"claim": "holding the claim", "calibrated": "true"},
        "settings": {"cor_forward_mm": 12.5, "cor_down_mm": null, "cor_right_mm": -4.0, "srs_export": true}
    })
}

/// OpenXR registered (Uninstall), the SteamVR compensation not (Install), the SteamVR runtime found.
fn setup_answer() -> Value {
    json!({
        "openxr": {"status": "Installed", "action": "Uninstall", "installed": true, "registered": true},
        "steamvr": {"status": "Not installed", "action": "Install", "registered": false},
        "steamvr_runtime": "/tmp/Steam VR"
    })
}

/// The screen on the VR status group, on the service fixture.
fn vr_screen(setup: Option<Value>) -> (MyApp, std::sync::Arc<VrService>) {
    let fake = FakeBackend::new();
    fake.docs.lock().unwrap()["compensation_sequence"] = json!([vr_status()]);
    let mut app = loaded(fake.clone());
    let service = std::sync::Arc::new(VrService {
        inner: fake,
        fail: std::sync::Mutex::new(false),
        setup: std::sync::Mutex::new(setup.map(Ok)),
    });
    app.backend = service.clone();
    navigate(&mut app, Screen::Compensation, STATUS_GROUP);
    drop(app.take_threads());
    (app, service)
}

fn calls_to(service: &VrService, prefix: &str) -> Vec<(Method, String, Option<Value>)> {
    service.inner.calls.lock().unwrap().iter().filter(|(_, path, _)| path.starts_with(prefix)).cloned().collect()
}

/// Runs the one queued install / uninstall job as its thread would, and gives the app back.
fn run_job(mut app: MyApp) -> MyApp {
    let mut jobs = app.take_threads();
    assert_eq!(jobs.len(), 1);
    let job = jobs.pop().unwrap();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(app));
    (job.run)(std::sync::Arc::downgrade(&shared));
    std::sync::Arc::try_unwrap(shared).ok().expect("the job keeps no app").into_inner().unwrap()
}

/// The component's actions offered: (install, uninstall).
fn component_actions(html: &str, key: &str) -> (bool, bool) {
    (html.contains(&format!(":install.{key}:\"")), html.contains(&format!(":uninstall.{key}:\"")))
}

#[test]
fn the_vr_status_shows_every_reading_and_follows_each_pass() {
    let (mut app, service) = vr_screen(None);
    rules(&mut app).unwrap();
    let html = main(&app);
    for line in [
        "Writing; 200.0 Hz; /tmp/rig-pose<",
        "SteamVR driver; PID 42; Live",
        "claim: holding the claim; calibrated: true",
        "Connected /dev/fake-imu; X: 1.0000  Y: 2.0000  Z: 3.0000; no gyro packets",
        "sway: 0.0000  surge: 0.0000  heave: 0.1000  yaw: 0.0000  roll: -0.5000  pitch: 0.2500",
        "sway: 0.0000  surge: 0.0000  heave: 0.0000  yaw: 3.0000  roll: 2.0000  pitch: 1.0000",
    ] {
        assert!(html.contains(line), "{line}");
    }
    let mut status = vr_status();
    status["rig_pose"]["error"] = json!("writer stopped");
    status["rig_pose"]["published"]["enabled"] = json!(false);
    status["rig_pose"]["published"]["age_ms"] = json!(12.5);
    status["consumer"]["live"] = json!(false);
    status["imu"]["rates"] = json!([4.0, 5.0, 6.0]);
    status["imu"]["age_ms"] = json!(7.5);
    service.inner.docs.lock().unwrap()["compensation_sequence"] = json!([status]);
    rules(&mut app).unwrap();
    let html = main(&app);
    assert!(html.contains("Writing; 200.0 Hz; /tmp/rig-pose; writer stopped; disabled; sample 12.5 ms old<"));
    assert!(html.contains("; gyro X: 4.0000  Y: 5.0000  Z: 6.0000 °/s; 7.5 ms old<"));
    assert!(html.contains("SteamVR driver; PID 42; Stale"));
    // The status cannot be read: the writer line says why; the next good read recovers.
    *service.fail.lock().unwrap() = true;
    rules(&mut app).unwrap();
    assert!(main(&app).contains("Status unavailable: VR service unavailable: compensation<"));
    *service.fail.lock().unwrap() = false;
    service.inner.docs.lock().unwrap()["compensation_sequence"] = json!([vr_status()]);
    rules(&mut app).unwrap();
    assert!(main(&app).contains("Writing; 200.0 Hz; /tmp/rig-pose<"));
    drop(app.take_threads());
}

#[test]
fn the_vr_status_is_read_only_while_the_screen_is_shown() {
    let (mut app, service) = vr_screen(None);
    let reads = |service: &VrService| {
        calls_to(service, "compensation").iter().filter(|(method, _, _)| *method == Method::Get).count()
    };
    navigate(&mut app, Screen::Hardware, 0);
    drop(app.take_threads());
    service.inner.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    assert_eq!(reads(&service), 0, "another screen");
    navigate(&mut app, Screen::Compensation, STATUS_GROUP);
    service.inner.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    let calls = calls_to(&service, "compensation");
    assert!(calls.iter().any(|(m, p, _)| *m == Method::Get && p == "compensation"));
    assert!(calls.iter().any(|(m, p, _)| *m == Method::Get && p == "compensation/setup"), "the set-up too");
    app.ui.mounted = false;
    service.inner.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    assert_eq!(reads(&service), 0, "the interface is not shown");
    drop(app.take_threads());
}

#[test]
fn failing_vr_actions_warn_with_the_current_reason() {
    let (mut app, service) = vr_screen(None);
    set_sensor(&service.inner, &mut app, "witmotion");
    *service.fail.lock().unwrap() = true;
    dispatch(&mut app, "action", &format!("{KEY}:recenter:"), "").unwrap();
    assert_eq!(
        crate::app::tests::alert_text(&app).as_deref(),
        Some("VR service unavailable: compensation/actions/recenter")
    );
    dispatch(&mut app, "action", &format!("{KEY}:vr-setup:"), "").unwrap();
    assert_eq!(crate::app::tests::alert_text(&app).as_deref(), Some("VR service unavailable: compensation"));
    dispatch(&mut app, "dialog-field", "cor_forward_mm", "12").unwrap();
    assert_eq!(crate::app::tests::alert_text(&app).as_deref(), Some("VR service unavailable: compensation/vr"));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    dispatch(&mut app, "action", &format!("{KEY}:pitch:0"), "").unwrap();
    assert!(dispatch(&mut app, "dialog-ok", "", "").is_err());
    assert_eq!(
        crate::app::tests::alert_text(&app).as_deref(),
        Some("VR service unavailable: compensation/actions/pitch_calibrate")
    );
    drop(app.take_threads());
}

#[test]
fn the_vr_dialog_shows_the_current_settings_and_the_seated_height() {
    let (mut app, service) = vr_screen(None);
    let open = |app: &mut MyApp| {
        dispatch(app, "action", &format!("{KEY}:vr-setup:"), "").unwrap();
        crate::ui::render(app, Region::Dialog)
    };
    let html = open(&mut app);
    assert!(tag(&html, "vr-cor_forward_mm").contains("value=\"12.5\""));
    assert!(tag(&html, "vr-cor_right_mm").contains("value=\"-4\""));
    let below = tag(&html, "vr-cor_down_mm");
    assert!(below.contains("value=\"1000\"") && below.contains(" disabled"), "Below follows the seated height");
    assert!(tag(&html, "vr-seated").contains(" checked") && tag(&html, "vr-export").contains(" checked"));
    // Below follows a changed seated height.
    dispatch(&mut app, "set", &format!("{KEY}:height"), "850").unwrap();
    assert!(tag(&crate::ui::render(&app, Region::Dialog), "vr-cor_down_mm").contains("value=\"850\""));
    // A fixed Below, export off.
    let mut status = vr_status();
    status["settings"]["cor_down_mm"] = json!(620.0);
    status["settings"]["srs_export"] = json!(false);
    service.inner.docs.lock().unwrap()["compensation_sequence"] = json!([status]);
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    let html = open(&mut app);
    let below = tag(&html, "vr-cor_down_mm");
    assert!(below.contains("value=\"620\"") && !below.contains(" disabled"));
    assert!(!tag(&html, "vr-seated").contains(" checked") && !tag(&html, "vr-export").contains(" checked"));
    drop(app.take_threads());
}

#[test]
fn the_vr_dialog_writes_finite_offsets_the_seated_height_and_the_export_then_recenter() {
    let (mut app, service) = vr_screen(None);
    dispatch(&mut app, "action", &format!("{KEY}:vr-setup:"), "").unwrap();
    let height = service.inner.docs.lock().unwrap()["settings"]["compensation"]["height"].clone();
    service.inner.calls.lock().unwrap().clear();
    for (field, value) in [
        ("cor_forward_mm", "-17.25"),
        ("cor_down_mm", "0"),
        ("cor_right_mm", "9000.5"),
        ("seated", "1"),
        ("seated", "0"),
        ("export", "0"),
    ] {
        dispatch(&mut app, "dialog-field", field, value).unwrap();
        if field == "seated" && value == "1" {
            let below = tag(&crate::ui::render(&app, Region::Dialog), "vr-cor_down_mm").to_owned();
            let shown = height.as_f64().unwrap().to_string();
            assert!(below.contains(&format!("value=\"{shown}\"")) && below.contains(" disabled"), "{below}");
        }
    }
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    dispatch(&mut app, "action", &format!("{KEY}:recenter:"), "").unwrap();
    let writes: Vec<_> =
        calls_to(&service, "compensation/").into_iter().filter(|(m, _, _)| *m != Method::Get).collect();
    let vr = |body: Value| (Method::Patch, "compensation/vr".to_owned(), Some(body));
    assert_eq!(
        writes,
        [
            vr(json!({"cor_forward_mm": -17.25})),
            vr(json!({"cor_down_mm": 0.0})),
            vr(json!({"cor_right_mm": 9000.5})),
            vr(json!({"cor_down_mm": null})),
            vr(json!({ "cor_down_mm": height })),
            vr(json!({"srs_export": false})),
            (Method::Post, "compensation/actions/recenter".to_owned(), Some(json!({}))),
        ]
    );
    // A distance that is not a finite number is refused and not sent.
    dispatch(&mut app, "action", &format!("{KEY}:vr-setup:"), "").unwrap();
    for invalid in ["", "NaN", "inf", "bad"] {
        crate::app::tests::dismiss_alerts(&mut app);
        dispatch(&mut app, "dialog-field", "cor_forward_mm", invalid).unwrap();
        assert_eq!(
            crate::app::tests::alert_text(&app).as_deref(),
            Some("Enter a finite distance in millimetres."),
            "{invalid:?}"
        );
    }
    assert_eq!(service.inner.patches("compensation/vr").len(), 5 + 1);
    drop(app.take_threads());
}

#[test]
fn the_seated_height_is_sent_as_its_number_and_a_bad_one_is_refused() {
    let (mut app, service) = vr_screen(None);
    app.set_setting(&["compensation", "height"], json!("1000"));
    dispatch(&mut app, "action", &format!("{KEY}:vr-setup:"), "").unwrap();
    assert!(tag(&crate::ui::render(&app, Region::Dialog), "vr-cor_down_mm").contains("value=\"1000\""));
    app.set_setting(&["compensation", "height"], json!("850.5"));
    assert!(tag(&crate::ui::render(&app, Region::Dialog), "vr-cor_down_mm").contains("value=\"850.5\""));
    service.inner.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-field", "seated", "0").unwrap();
    assert_eq!(service.inner.patches("compensation/vr"), [json!({"cor_down_mm": 850.5})]);
    assert!(!tag(&crate::ui::render(&app, Region::Dialog), "vr-seated").contains(" checked"));
    dispatch(&mut app, "dialog-field", "seated", "1").unwrap();
    for height in ["NaN", "inf", "bad"] {
        app.set_setting(&["compensation", "height"], json!(height));
        service.inner.calls.lock().unwrap().clear();
        crate::app::tests::dismiss_alerts(&mut app);
        dispatch(&mut app, "dialog-field", "seated", "0").unwrap();
        assert_eq!(service.inner.patches("compensation/vr"), Vec::<Value>::new(), "{height}");
        assert!(tag(&crate::ui::render(&app, Region::Dialog), "vr-seated").contains(" checked"), "{height}");
        assert!(crate::app::tests::alert_text(&app).is_some(), "{height}");
    }
    drop(app.take_threads());
}

#[test]
fn the_vr_setup_lists_the_bundled_components_and_their_actions() {
    let (mut app, _) = vr_screen(Some(setup_answer()));
    let html = main(&app);
    for name in [">OpenXR<", ">SteamVR runtime<", ">SteamVR<"] {
        assert!(html.contains(name), "{name}");
    }
    assert!(html.contains("<span>Installed</span> <button") && html.contains("<span>Not installed</span> <button"));
    assert_eq!(component_actions(&html, "openxr"), (false, true));
    assert_eq!(component_actions(&html, "steamvr_compensation"), (true, false));
    assert_eq!(component_actions(&html, "steamvr"), (false, false));
    for absent in [
        "Instructions",
        "Re-enable",
        "BuzzteeBear",
        "ovrmc.dschadu",
        "freshdesk",
        "Download",
        "OPENRIGCUE_XR_LAYER_ENABLE",
    ] {
        assert!(!html.contains(absent), "{absent}");
    }
    // Without the SteamVR runtime it is not installed.
    let mut answer = setup_answer();
    answer.as_object_mut().unwrap().remove("steamvr_runtime");
    let (mut without, _) = vr_screen(Some(answer));
    assert!(
        main(&without).contains("SteamVR runtime</strong></div><div class=\"control-body\"><span>Not installed</span>")
    );
    drop(without.take_threads());
    drop(app.take_threads());
}

#[test]
fn install_runs_only_its_component_on_a_thread() {
    let (mut app, service) = vr_screen(Some(setup_answer()));
    service.inner.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", &format!("{KEY}:install.steamvr_compensation:"), "").unwrap();
    assert!(main(&app).contains("install.steamvr_compensation:\" disabled aria-busy=\"true\""));
    assert!(calls_to(&service, "compensation/setup").iter().all(|(m, _, _)| *m != Method::Post), "not yet");
    let app = run_job(app);
    let posts: Vec<_> =
        calls_to(&service, "compensation/setup").into_iter().filter(|(m, _, _)| *m == Method::Post).collect();
    assert_eq!(posts, [(Method::Post, "compensation/setup/steamvr".to_owned(), Some(json!({})))]);
    let html = main(&app);
    assert!(!html.contains("aria-busy"));
    assert!(html.contains("SteamVR</strong></div><div class=\"control-body\"><span>Not installed</span>"));
    let mut app = app;
    drop(app.take_threads());
}

#[test]
fn a_failed_set_up_read_shows_unavailable_without_actions_until_it_recovers() {
    let (mut app, service) = vr_screen(Some(setup_answer()));
    *service.setup.lock().unwrap() = Some(Err(Failure { status: 503, detail: "installer status failed".into() }));
    dispatch(&mut app, "action", &format!("{KEY}:check:"), "").unwrap();
    let html = main(&app);
    assert!(html.contains("OpenXR</strong></div><div class=\"control-body\"><span>Status unavailable</span>"));
    for key in ["openxr", "steamvr_compensation"] {
        assert_eq!(component_actions(&html, key), (false, false), "{key}");
    }
    // A hidden action does nothing.
    dispatch(&mut app, "action", &format!("{KEY}:uninstall.openxr:"), "").unwrap();
    assert!(app.take_threads().is_empty());
    *service.setup.lock().unwrap() = Some(Ok(setup_answer()));
    dispatch(&mut app, "action", &format!("{KEY}:check:"), "").unwrap();
    let html = main(&app);
    assert_eq!(component_actions(&html, "openxr"), (false, true));
    assert_eq!(component_actions(&html, "steamvr_compensation"), (true, false));
    drop(app.take_threads());
}

#[test]
fn a_finished_operation_shows_the_new_set_up_and_a_failure_says_why() {
    let (mut app, service) = vr_screen(Some(setup_answer()));
    service.inner.calls.lock().unwrap().clear();
    finish_operation(
        &mut app,
        "openxr",
        Ok(json!({
            "openxr": {"status": "Installed", "action": "Uninstall", "registered": true},
            "steamvr": {"status": "Not installed", "action": "Install", "registered": false},
            "steamvr_runtime": "/tmp/SteamVR"
        })),
    );
    assert!(main(&app).contains("OpenXR</strong></div><div class=\"control-body\"><span>Installed</span>"));
    assert!(calls_to(&service, "compensation").iter().all(|(m, _, _)| *m != Method::Post), "no further operation");
    assert!(crate::app::tests::notice_texts(&app).iter().all(|(_, text)| !text.contains("launch option")));
    finish_operation(
        &mut app,
        "openxr",
        Err(Failure { status: 500, detail: "Cannot install: permission denied".into() }),
    );
    assert_eq!(crate::app::tests::alert_text(&app).as_deref(), Some("Cannot install: permission denied"));
    assert!(calls_to(&service, "compensation/setup")
        .iter()
        .any(|(m, p, _)| *m == Method::Get && p == "compensation/setup"));
    drop(app.take_threads());
}

#[test]
fn uninstall_runs_on_a_thread_and_the_answer_swaps_the_actions() {
    let (mut app, service) = vr_screen(Some(setup_answer()));
    dispatch(&mut app, "action", &format!("{KEY}:uninstall.openxr:"), "").unwrap();
    *service.setup.lock().unwrap() = Some(Ok(json!({
        "openxr": {"status": "Not installed", "action": "Install", "registered": false},
        "steamvr": {"status": "Installed", "action": "Uninstall", "registered": true},
        "steamvr_runtime": "/tmp/SteamVR"
    })));
    service.inner.calls.lock().unwrap().clear();
    let mut app = run_job(app);
    assert_eq!(
        calls_to(&service, "compensation/setup"),
        [(Method::Post, "compensation/setup/openxr/uninstall".to_owned(), Some(json!({})))]
    );
    let html = main(&app);
    assert_eq!(component_actions(&html, "openxr"), (true, false));
    assert_eq!(component_actions(&html, "steamvr_compensation"), (false, true));
    assert!(!html.contains("OPENRIGCUE_XR_LAYER_ENABLE=1"));
    drop(app.take_threads());
}

#[test]
fn a_failed_install_says_why_and_offers_no_action_until_the_status_is_known() {
    let (mut app, service) = vr_screen(Some(setup_answer()));
    dispatch(&mut app, "action", &format!("{KEY}:install.steamvr_compensation:"), "").unwrap();
    *service.setup.lock().unwrap() = Some(Err(Failure { status: 500, detail: "Current registration error".into() }));
    service.inner.calls.lock().unwrap().clear();
    let mut app = run_job(app);
    assert!(calls_to(&service, "compensation/setup")
        .iter()
        .any(|(m, p, _)| *m == Method::Post && p == "compensation/setup/steamvr"));
    assert_eq!(crate::app::tests::alert_text(&app).as_deref(), Some("Current registration error"));
    let html = main(&app);
    assert!(!html.contains("aria-busy"));
    for key in ["openxr", "steamvr_compensation"] {
        assert_eq!(component_actions(&html, key), (false, false), "{key}");
    }
    drop(app.take_threads());
}

#[test]
fn install_and_uninstall_are_exclusive_and_a_repeat_is_ignored() {
    let (mut app, _) = vr_screen(Some(setup_answer()));
    // The registered OpenXR offers Uninstall only: Install does nothing.
    dispatch(&mut app, "action", &format!("{KEY}:install.openxr:"), "").unwrap();
    assert!(app.take_threads().is_empty(), "hidden install cannot redeploy an installed target");
    dispatch(&mut app, "action", &format!("{KEY}:uninstall.openxr:"), "").unwrap();
    assert_eq!(app.take_threads().len(), 1);
    let html = main(&app);
    assert!(html.contains("uninstall.openxr:\" disabled aria-busy=\"true\""), "busy while it runs");
    assert!(!html.contains(":install.openxr:\""));
    dispatch(&mut app, "action", &format!("{KEY}:uninstall.openxr:"), "").unwrap();
    assert!(app.take_threads().is_empty(), "a repeat is ignored");
    finish_operation(
        &mut app,
        "openxr",
        Ok(json!({
            "openxr": {"status": "Not installed", "installed": true, "registered": false, "action": "Install"},
            "steamvr": {"status": "Installed", "installed": true, "registered": true, "action": "Uninstall"},
            "steamvr_runtime": "/tmp/SteamVR"
        })),
    );
    let html = main(&app);
    // Installed but not registered is not deployed: Install.
    assert_eq!(component_actions(&html, "openxr"), (true, false));
    assert_eq!(component_actions(&html, "steamvr_compensation"), (false, true));
    dispatch(&mut app, "action", &format!("{KEY}:uninstall.openxr:"), "").unwrap();
    assert!(app.take_threads().is_empty(), "hidden uninstall cannot act on an absent target");
    drop(app.take_threads());
}

#[test]
fn a_failed_uninstall_keeps_the_registered_targets_uninstall() {
    let (mut app, service) = vr_screen(Some(setup_answer()));
    dispatch(&mut app, "action", &format!("{KEY}:uninstall.openxr:"), "").unwrap();
    assert_eq!(app.take_threads().len(), 1);
    *service.setup.lock().unwrap() = Some(Ok(json!({
        "openxr": {"status": "Setup failed: runtime refused removal", "registered": true, "action": "Uninstall"},
        "steamvr": {"status": "SteamVR not found", "registered": false, "action": null}
    })));
    finish_operation(&mut app, "openxr", Err(Failure { status: 500, detail: "runtime refused removal".into() }));
    let html = main(&app);
    assert_eq!(component_actions(&html, "openxr"), (false, true));
    assert_eq!(component_actions(&html, "steamvr_compensation"), (false, false));
    assert!(html.contains("<span>Setup failed: runtime refused removal</span>"));
    assert_eq!(crate::app::tests::alert_text(&app).as_deref(), Some("runtime refused removal"));
    drop(app.take_threads());
}

#[test]
fn an_open_vr_dialog_follows_the_settings_the_status_reports() {
    let (mut app, service) = vr_screen(None);
    dispatch(&mut app, "action", &format!("{KEY}:vr-setup:"), "").unwrap();
    assert!(tag(&crate::ui::render(&app, Region::Dialog), "vr-cor_forward_mm").contains("value=\"12.5\""));
    // Another client changed the centre of rotation and the export: the next pass shows them.
    let mut status = vr_status();
    status["settings"]["cor_forward_mm"] = json!(-30.0);
    status["settings"]["cor_down_mm"] = json!(640.0);
    status["settings"]["srs_export"] = json!(false);
    service.inner.docs.lock().unwrap()["compensation_sequence"] = json!([status]);
    rules(&mut app).unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(tag(&html, "vr-cor_forward_mm").contains("value=\"-30\""), "{html}");
    let below = tag(&html, "vr-cor_down_mm");
    assert!(below.contains("value=\"640\"") && !below.contains(" disabled"));
    assert!(!tag(&html, "vr-seated").contains(" checked") && !tag(&html, "vr-export").contains(" checked"));
    // An unchanged status leaves the dialog as it is (nothing is drawn again while the user types).
    let before = crate::ui::render(&app, Region::Dialog);
    rules(&mut app).unwrap();
    assert_eq!(crate::ui::render(&app, Region::Dialog), before);
    drop(app.take_threads());
}

/// The VR service with the live status read held at a gate until the test releases it.
struct GatedService {
    inner: std::sync::Arc<VrService>,
    entered: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Backend for GatedService {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if method == Method::Get && path == "compensation" {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                entered.send(()).unwrap();
                let release = self.release.lock().unwrap().take().unwrap();
                release.recv().unwrap();
            }
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        self.inner.native_setting(section, key)
    }
    fn ui_actions(&self) -> Vec<String> {
        self.inner.ui_actions()
    }
    fn clear_ui_actions(&self) {
        self.inner.clear_ui_actions()
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
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
fn the_main_loop_reads_the_vr_status_without_holding_the_application() {
    let (mut app, service) = vr_screen(None);
    let (entered_send, entered) = std::sync::mpsc::channel();
    let (release, release_receive) = std::sync::mpsc::channel();
    app.backend = std::sync::Arc::new(GatedService {
        inner: service.clone(),
        entered: std::sync::Mutex::new(Some(entered_send)),
        release: std::sync::Mutex::new(Some(release_receive)),
    });
    let shared = std::sync::Arc::new(std::sync::Mutex::new(app));
    let worker = {
        let shared = shared.clone();
        std::thread::spawn(move || rules_shared(&shared))
    };
    entered.recv().unwrap();
    // While the service is still answering, the application is free for the browser and the timers.
    assert!(shared.try_lock().is_ok(), "the application is not held during the read");
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    let mut app = std::sync::Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap();
    assert!(matches!(app.ui.devices.compensation.status, Some(Ok(_))), "the read was taken");
    drop(app.take_threads());
}

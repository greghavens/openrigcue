//! Tests of Hardware → Hardware: its groups and rows, every value it stores, the rules by device, model and
//! game, the device tests, the motion and belt tests, the controller dialogs and the main loop's work.

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Method};
use crate::host::UiApp;
use crate::ui::dialogs_hardware::{self as dh, Device, Test};
use crate::ui::{dispatch, navigate, Theme};

fn hardware(backend: &std::sync::Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Hardware, 0);
    drop(app.take_threads());
    app
}

/// Shows a settings group (by its title).
fn group(app: &mut MyApp, title: &str) -> String {
    let index = spec::hardware_page().sections.iter().position(|s| s.title == title).unwrap();
    dispatch(app, "section", "", &index.to_string()).unwrap();
    main(app)
}

fn native(backend: &FakeBackend, section: &str, key: &str) -> LegacyValue {
    backend.native_setting(section, key).unwrap()
}

fn tune(backend: &FakeBackend, path: &[&str]) -> serde_json::Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["tune"].clone(), |value, key| value[*key].clone())
}

fn control(backend: &FakeBackend, path: &[&str]) -> serde_json::Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs["status"]["controls"].clone(), |value, key| value[*key].clone())
}

fn called(backend: &FakeBackend, method: Method, path: &str) -> Vec<Option<serde_json::Value>> {
    backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, p, _)| *m == method && p == path)
        .map(|(.., b)| b.clone())
        .collect()
}

fn option_index(html: &str, id: &str, shown: &str) -> String {
    let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("{id} in {html}"));
    let select = &html[start..html[start..].find("</select>").unwrap() + start];
    let at = select.find(&format!(">{shown}</option>")).unwrap_or_else(|| panic!("{shown} in {select}"));
    let value = select[..at].rsplit("value=\"").next().unwrap();
    value.split('"').next().unwrap().to_owned()
}

#[test]
fn every_group_is_offered_with_hapticr_only_when_enabled() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let html = main(&app);
    for title in ["Intellibox", "Shakers", "Motion", "Motion Compensation (WITMotion)", "Belt", "SIM3D", "HUD", "IFlag"]
    {
        assert!(html.contains(&format!(">{title}</option>")), "{title}");
    }
    assert!(!html.contains(">HapticR</option>"));
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    app.refresh_docs();
    assert!(main(&app).contains(">HapticR</option>"));
    // Each theme draws the screen with its own heading text.
    for theme in Theme::ALL {
        app.ui.theme = theme;
        let html = main(&app);
        assert!(html.contains("data-arg=\"Hardware:auto_search_INTELLIBOX\""), "{theme:?}");
        let title = if theme.capitals() { "INTELLIBOX" } else { "Intellibox" };
        assert!(html.contains(&format!("<h2>{title}</h2>")), "{theme:?}");
    }
    // Com Port rows name Windows ports: they are offered only where this program runs on Windows.
    assert_eq!(html.contains("Hardware:com_port_INTELLIBOX"), cfg!(windows));
    drop(app.take_threads());
}

#[test]
fn intellibox_values_are_stored_with_their_rules_elsewhere() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let html = group(&mut app, "Intellibox");
    dispatch(&mut app, "set", "Hardware:auto_search_INTELLIBOX", "0").unwrap();
    assert_eq!(native(&backend, "INTELLIBOX", "auto_search"), LegacyValue::Bool(false));
    // A Blower limits the Wind power range to 35–90 %.
    dispatch(
        &mut app,
        "set",
        "Hardware:wind_type",
        &option_index(&html, "c-hardware-wind_type", "Blower (discontinued)"),
    )
    .unwrap();
    assert_eq!(native(&backend, "INTELLIBOX", "wind_type"), LegacyValue::Str("B".into()));
    assert_eq!(
        (tune(&backend, &["power_wind_min_speed_perc"]), tune(&backend, &["power_wind_max_speed_perc"])),
        (json!(35.0), json!(90.0))
    );
    dispatch(&mut app, "set", "Hardware:wind_type", "0").unwrap();
    assert_eq!(tune(&backend, &["power_wind_max_speed_perc"]), json!(100.0));
    // One fan turns Wind Curving off with a curving effect of 1.
    dispatch(&mut app, "set", "Hardware:nbr_fans", "0").unwrap();
    assert_eq!(native(&backend, "INTELLIBOX", "nbr_fans"), LegacyValue::Str("1".into()));
    assert_eq!((tune(&backend, &["wind_curving"]), tune(&backend, &["curving_effect"])), (json!(0), json!(1)));
    dispatch(&mut app, "set", "Hardware:nbr_fans", "1").unwrap();
    assert_eq!(native(&backend, "INTELLIBOX", "nbr_fans"), LegacyValue::Str("2".into()));
    dispatch(&mut app, "set", "Hardware:led_model", "1").unwrap();
    assert_eq!(native(&backend, "LED", "model"), LegacyValue::Str("GRB".into()));
    // The LED count keeps the Tuning → LED segments within it.
    backend.native_setting("LED", "segment1_end").unwrap();
    app.set_setting_native_checked("LED", "segment1_start", &LegacyValue::Int(20)).unwrap();
    app.set_setting_native_checked("LED", "segment1_end", &LegacyValue::Int(40)).unwrap();
    dispatch(&mut app, "set", "Hardware:nbr_leds", "30").unwrap();
    assert_eq!(native(&backend, "LED", "nbr_leds"), LegacyValue::Int(30));
    assert_eq!(native(&backend, "LED", "segment1_end"), LegacyValue::Int(30));
    assert_eq!(native(&backend, "LED", "segment1_start"), LegacyValue::Int(20));
    dispatch(&mut app, "set", "Hardware:brightness", "55").unwrap();
    assert_eq!(native(&backend, "LED", "brightness"), LegacyValue::Int(55));
    // The shift light brightness is part of the profile.
    app.ui.snapshot = Some(app.docs.tune.clone());
    dispatch(&mut app, "set", "Hardware:shift_light_dim_perc", "42").unwrap();
    assert_eq!(tune(&backend, &["gear_shift_dim_perc"]), json!(42));
    assert!(crate::ui::dirty(&app));
    assert!(compared(&app).iter().any(|(_, path)| path == &["gear_shift_dim_perc"]));
    drop(app.take_threads());
}

#[test]
fn intellibox_tests_hold_always_on_and_restore_the_outputs() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    group(&mut app, "Intellibox");
    dispatch(&mut app, "action", "Hardware:wind_type:0", "").unwrap();
    assert_eq!(dh::device(&app), Some(&Device::Test(Test::Wind)));
    assert!(crate::ui::render(&app, Region::Dialog).contains(">Start test<"));
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert_eq!(control(&backend, &["switches", "wind"]), json!("always_on"));
    assert_eq!(control(&backend, &["switches", "wind_center"]), json!("always_on"));
    assert_eq!(app.take_threads().len(), 1, "the test ends on its own timer");
    // While it runs, the other tests are disabled.
    assert!(main(&app).contains("data-arg=\"Hardware:nbr_leds:0\" disabled"));
    dispatch(&mut app, "dialog-button", "stop", "").unwrap();
    assert_eq!(control(&backend, &["switches", "wind"]), json!("auto"));
    assert!(app.ui.devices.hardware.test.is_none());
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn short_tests_follow_their_device_rules() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = hardware(&backend);
    let html = group(&mut app, "IFlag");
    assert!(html.contains("id=\"c-hardware-iflag_test\"") && html.contains(" disabled"), "iFlag not connected");
    backend.docs.lock().unwrap()["status"]["accessories"]["iflag"] = json!("Connected");
    app.refresh_docs();
    let html = main(&app);
    assert!(!html.contains("data-arg=\"Hardware:iflag_test:\" disabled"));
    dispatch(&mut app, "action", "Hardware:iflag_test:", "").unwrap();
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert_eq!(control(&backend, &["tests", "iflag_test"]), json!(true));
    drop(app.take_threads());
    finish_test(&mut app).unwrap();
    assert_eq!(control(&backend, &["tests", "iflag_test"]), json!(false));
    // HapticR: both wheels of the front and rear test.
    backend.docs.lock().unwrap()["status"]["accessories"]["hapticr"] = json!("Connected");
    app.refresh_docs();
    group(&mut app, "HapticR");
    dispatch(&mut app, "action", "Hardware:haptic_test_rear:0", "").unwrap();
    assert_eq!(dh::device(&app), Some(&Device::Test(Test::Haptic(4))));
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert_eq!(control(&backend, &["tests", "haptic_test"]), json!(4));
    finish_test(&mut app).unwrap();
    assert_eq!(control(&backend, &["tests", "haptic_test"]), json!(0));
    // SIM3D: the port rows follow the model, the tests a connected device.
    app.set_setting_native_checked("SIM3D", "model", &LegacyValue::Str("3 Ports".into())).unwrap();
    let html = group(&mut app, "SIM3D");
    assert!(html.contains("Hardware:rumble_port3\"") && !html.contains("Hardware:rumble_port4\""));
    assert!(html.contains("data-arg=\"Hardware:rumble_port1:0\" disabled"));
    backend.docs.lock().unwrap()["status"]["accessories"]["sim3d"] = json!("Connected");
    app.refresh_docs();
    assert!(!main(&app).contains("data-arg=\"Hardware:rumble_port1:0\" disabled"));
    dispatch(&mut app, "action", "Hardware:rumble_port2:0", "").unwrap();
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert_eq!(control(&backend, &["tests", "rumble_test"]), json!(2));
    finish_test(&mut app).unwrap();
    assert_eq!(control(&backend, &["tests", "rumble_test"]), json!(0));
    dispatch(&mut app, "set", "Hardware:rumble_port1", "3").unwrap();
    assert_eq!(native(&backend, "SIM3D", "port1"), LegacyValue::Str("Clutch".into()));
    drop(app.take_threads());
}

#[test]
fn shakers_store_the_device_layout_and_volumes() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let html = group(&mut app, "Shakers");
    // Without a sound device the layout and the speakers are disabled.
    assert!(html.contains("data-arg=\"Hardware:shakers_profile:0\" disabled"));
    assert!(html.contains("data-arg=\"Hardware:fl:0\" disabled"));
    assert!(html.contains(">USB Sound Device</option>"), "device names are shown as named");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Hardware:soundcard", &option_index(&html, "c-hardware-soundcard", "USB Sound Device"))
        .unwrap();
    assert_eq!(native(&backend, "SHAKEBOX", "soundcard"), LegacyValue::Str("USB Sound Device".into()));
    assert!(!called(&backend, Method::Put, "shakers/soundcard").is_empty(), "the device is opened");
    dispatch(&mut app, "set", "Hardware:sub", "12").unwrap();
    assert_eq!(tune(&backend, &["shaker_volume", "sub"]), json!(12));
    // The layout: stored, its effects loaded and the surround checked again.
    let html = main(&app);
    let layouts: Vec<String> = layouts(&app).into_iter().map(|c| c.label).collect();
    let other = layouts.iter().find(|name| **name != setting_text(&app, "SHAKEBOX", "profile")).unwrap().clone();
    backend.calls.lock().unwrap().clear();
    let index = layouts.iter().position(|name| *name == other).unwrap();
    assert!(html.contains("c-hardware-shakers_profile"));
    dispatch(&mut app, "set", "Hardware:shakers_profile", &index.to_string()).unwrap();
    assert_eq!(native(&backend, "SHAKEBOX", "profile"), LegacyValue::Str(other));
    assert!(!called(&backend, Method::Post, "shakers/effects/reload").is_empty());
    assert!(!called(&backend, Method::Post, "shakers/surround-recheck").is_empty());
    // The speaker test plays the speaker's sound.
    dispatch(&mut app, "action", "Hardware:fl:0", "").unwrap();
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert!(!called(&backend, Method::Post, "shakers/speakers/FL/test").is_empty());
    dispatch(&mut app, "set", "Hardware:use_srs_sounds", "1").unwrap();
    assert!(backend.patches("settings").iter().any(|p| p["shakers"]["use_srs_sounds"] == json!(true)));
    // The SRS installer import.
    dispatch(&mut app, "action", "Hardware:srs_import:", "").unwrap();
    assert!(crate::app::tests::alert_text(&app).unwrap().contains("Enter the path"));
    dispatch(&mut app, "set", "Hardware:srs_path", "/data/SRS").unwrap();
    dispatch(&mut app, "action", "Hardware:srs_import:", "").unwrap();
    assert_eq!(called(&backend, Method::Post, "srs-import"), vec![Some(json!({"path": "/data/SRS"}))]);
    drop(app.take_threads());
}

#[test]
fn speaker_layouts_are_copied_and_removed_through_their_dialogs() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    group(&mut app, "Shakers");
    let source = setting_text(&app, "SHAKEBOX", "profile");
    dispatch(&mut app, "action", "Hardware:shakers_profile:0", "").unwrap();
    let Some(Device::Duplicate { name, .. }) = dh::device(&app).cloned() else { panic!("duplicate dialog") };
    assert!(name.ends_with("(1)"), "{name}");
    assert!(crate::ui::render(&app, Region::Dialog).contains("maxlength=\"30\""));
    // An existing name is refused in the dialog.
    dispatch(&mut app, "dialog-field", "name", &source).unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("A profile with this name exists already"));
    dispatch(&mut app, "dialog-field", "name", "My layout").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(
        called(&backend, Method::Post, "shakers/rows/copy"),
        vec![Some(json!({"source": source, "name": "My layout"}))]
    );
    assert_eq!(native(&backend, "SHAKEBOX", "profile"), LegacyValue::Str("My layout".into()));
    dispatch(&mut app, "action", "Hardware:shakers_profile:1", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("My layout"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(called(&backend, Method::Post, "shakers/rows/delete"), vec![Some(json!({"name": "My layout"}))]);
    assert_eq!(native(&backend, "SHAKEBOX", "profile"), LegacyValue::Str(DEFAULT_LAYOUT.into()));
    drop(app.take_threads());
}

#[test]
fn the_speaker_matrix_sends_the_speakers_levels() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    app.set_setting_native_checked("SHAKEBOX", "soundcard", &LegacyValue::Str("USB Sound Device".into())).unwrap();
    dispatch(&mut app, "action", "Hardware:fl:1", "").unwrap();
    let Some(Device::Matrix { speaker, levels }) = dh::device(&app).cloned() else { panic!("matrix dialog") };
    assert_eq!(speaker, "FL");
    let effect = levels.as_dict().unwrap().iter().next().unwrap().0.to_text();
    dispatch(&mut app, "dialog-field", &format!("level:{effect}"), "7").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let sent = called(&backend, Method::Put, "shakers/speakers/FL/matrix");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].as_ref().unwrap()[&effect], json!(7));
    drop(app.take_threads());
}

#[test]
fn motion_model_rules_and_cross_screen_writes() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let html = group(&mut app, "Motion");
    assert!(html.contains("Hardware:motion_manufacturer\"") && html.contains("Hardware:motion_model\""));
    // A manufacturer brings its refresh rate and its first model.
    let manufacturer = crate::catalog::manufacturers().into_iter().find(|m| m.name == "PROSIMU").unwrap();
    let index = crate::catalog::manufacturers().iter().position(|m| m.name == "PROSIMU").unwrap();
    dispatch(&mut app, "set", "Hardware:motion_manufacturer", &index.to_string()).unwrap();
    assert_eq!(
        native(&backend, "MOTIONBOX", "refresh_rate"),
        LegacyValue::Int(i64::from(manufacturer.default_refresh_rate))
    );
    let first = crate::catalog::models().iter().find(|m| m.manufacturer == "PROSIMU").unwrap();
    assert_eq!(native(&backend, "MOTIONBOX", "model"), LegacyValue::Str(first.name.clone()));
    // A model without "2 TL" zeroes and turns off the front traction loss.
    app.set_tune_native_checked(&["min_traction_loss_front_activation"], &LegacyValue::Int(30)).unwrap();
    let model = crate::catalog::models().iter().find(|m| !m.name.contains("2 TL") && m.name != "None").unwrap();
    let models: Vec<String> = models_of(&app, &model.manufacturer).iter().map(|m| m.name.clone()).collect();
    choose_model(&mut app, &model.name).unwrap();
    assert_eq!(tune(&backend, &["min_traction_loss_front_activation"]), json!(0));
    assert_eq!(tune(&backend, &["motion_switch", "traction_loss_front"]), json!(false));
    assert!(models.contains(&model.name));
    if let Some(two) = crate::catalog::models().iter().find(|m| m.name.contains("2 TL")) {
        choose_model(&mut app, &two.name).unwrap();
        assert_eq!(tune(&backend, &["motion_switch", "traction_loss_front"]), json!(true));
        assert!(group(&mut app, "Motion").contains("Hardware:traction_loss_front\""));
    }
    // A model without IntelliComp keeps the compensation sensor at WITMotion.
    app.set_setting_native_checked("MOTION_COMPENSATION", "type", &LegacyValue::Str("V".into())).unwrap();
    let plain = crate::catalog::models().iter().find(|m| !m.enable_intellicomp && m.name != "None").unwrap();
    choose_model(&mut app, &plain.name).unwrap();
    assert_eq!(native(&backend, "MOTION_COMPENSATION", "type"), LegacyValue::Str("W".into()));
    // Switch rows.
    dispatch(&mut app, "set", "Hardware:auto_smoothing_low_speed", "1").unwrap();
    assert_eq!(native(&backend, "MOTIONBOX", "auto_smoothing_low_speed"), LegacyValue::Int(1));
    drop(app.take_threads());
}

#[test]
fn manual_ports_and_the_controller_buttons_follow_the_model_and_connection() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["hardware"]["serial_ports"] = json!([
        {"device": "/dev/ttyUSB0", "hwid": "USB VID:PID=0403:6001"},
        {"device": "/dev/ttyACM3", "hwid": "USB VID:PID=2341:0043"}
    ]);
    let mut app = hardware(&backend);
    let vnm = crate::catalog::models().iter().find(|m| m.manufacturer == "VNM SIMULATION" && m.manual_com_port);
    if let Some(vnm) = vnm {
        choose_model(&mut app, &vnm.name).unwrap();
        let html = group(&mut app, "Motion");
        assert!(
            html.contains(">/dev/ttyUSB0</option>") && html.contains(">/dev/ttyACM3</option>"),
            "VNM lists every free port"
        );
        assert!(html.contains("<strong>Com Port</strong>"));
        dispatch(
            &mut app,
            "set",
            "Hardware:motion_port",
            &option_index(&html, "c-hardware-motion_port", "/dev/ttyACM3"),
        )
        .unwrap();
        assert_eq!(native(&backend, "VNM SIMULATION", "vnm_com_port"), LegacyValue::Str("/dev/ttyACM3".into()));
        // The controller button shows while the platform is connected.
        assert!(!html.contains("data-arg=\"Hardware:motion_model:4\""));
        backend.docs.lock().unwrap()["status"]["motion"]["link"]["status"] = json!("Connected");
        app.refresh_docs();
        assert!(main(&app).contains("data-arg=\"Hardware:motion_model:4\""));
    }
    let prosimu = crate::catalog::models().iter().find(|m| m.manufacturer == "PROSIMU" && m.manual_com_port).unwrap();
    choose_model(&mut app, &prosimu.name).unwrap();
    let html = group(&mut app, "Motion");
    assert!(html.contains(">/dev/ttyUSB0</option>") && !html.contains(">/dev/ttyACM3</option>"), "PROSIMU: FTDI only");
    drop(app.take_threads());
}

#[test]
fn the_motion_test_runs_only_while_allowed() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let model = crate::catalog::models().iter().find(|m| m.name != "None").unwrap();
    choose_model(&mut app, &model.name).unwrap();
    let html = group(&mut app, "Motion");
    assert!(html.contains("id=\"c-hardware-motion_test\"") && html.contains(">Start Test<"));
    assert!(html.contains("data-arg=\"Hardware:motion_test:\" disabled"), "not connected");
    backend.docs.lock().unwrap()["status"]["motion"]["link"]["status"] = json!("Connected");
    app.refresh_docs();
    dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(true));
    let html = main(&app);
    assert!(html.contains(">Stop Test<") && !html.contains("data-arg=\"Hardware:pitch\" data-output=\"c-hardware-pitch-out\" data-template=\"{}\" aria-describedby=\"c-hardware-pitch-hint\" disabled"));
    dispatch(&mut app, "set", "Hardware:pitch", "40").unwrap();
    assert_eq!(control(&backend, &["tests", "motion_sliders", "pitch"]), json!(40));
    // A running game stops the test: the sliders go back to 0.
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("acc");
    app.refresh_docs();
    rules(&mut app).unwrap();
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(false));
    assert_eq!(control(&backend, &["tests", "motion_sliders", "pitch"]), json!(0));
    drop(app.take_threads());
}

#[test]
fn the_belt_test_and_its_rules() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["accessories"]["belt"] = json!("Connected");
    let mut app = hardware(&backend);
    let html = group(&mut app, "Belt");
    assert!(!html.contains("data-arg=\"Hardware:belt_test:\" disabled"));
    dispatch(&mut app, "action", "Hardware:belt_test:", "").unwrap();
    assert_eq!(
        (control(&backend, &["switches", "belt"]), control(&backend, &["tests", "belt_test"])),
        (json!(true), json!(true))
    );
    dispatch(&mut app, "set", "Hardware:belt_test_slider", "17").unwrap();
    assert_eq!(control(&backend, &["tests", "belt_test_value"]), json!(17));
    dispatch(&mut app, "action", "Hardware:belt_test:", "").unwrap();
    assert_eq!(control(&backend, &["tests", "belt_test"]), json!(false));
    assert_eq!(control(&backend, &["tests", "belt_homing"]), json!(0));
    assert_eq!(control(&backend, &["tests", "belt_test_value"]), json!(0));
    dispatch(&mut app, "set", "Hardware:belt_calibration_power_slider", "61").unwrap();
    assert_eq!(native(&backend, "BELT_TENSIONER", "belt_calibration_factor"), LegacyValue::Int(61));
    // The belt switched off ends its test and calibration.
    app.set_control_checked(&["tests", "belt_homing"], json!(1)).unwrap();
    app.set_control_checked(&["switches", "belt"], json!(false)).unwrap();
    rules(&mut app).unwrap();
    assert_eq!(control(&backend, &["tests", "belt_homing"]), json!(0));
    drop(app.take_threads());
}

#[test]
fn iflag_and_hud_values_are_stored() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    group(&mut app, "IFlag");
    dispatch(&mut app, "set", "Hardware:iflag_dim_perc", "70").unwrap();
    assert_eq!(native(&backend, "IFLAG", "brightness"), LegacyValue::Int(70));
    dispatch(&mut app, "set", "Hardware:iflag_orientation", "3").unwrap();
    assert_eq!(native(&backend, "IFLAG", "rotation"), LegacyValue::Int(180));
    dispatch(&mut app, "set", "Hardware:iflag_mirror", "1").unwrap();
    assert!(native(&backend, "IFLAG", "mirror").is_truthy());
    dispatch(&mut app, "set", "Hardware:hud_model", "1").unwrap();
    assert_eq!(native(&backend, "HUD", "model"), LegacyValue::Str("B".into()));
    drop(app.take_threads());
}

fn thanos_backend(controller: &str) -> std::sync::Arc<FakeBackend> {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["motion"]["link"] = json!({"status": "Connected", "ports": {"a": "/dev/ttyUSB0"}});
        let mut config = serde_json::Map::new();
        for port in 1..5 {
            config.insert(format!("stroke{port}"), json!(15));
            config.insert(format!("lead_screw{port}"), json!(2));
            config.insert(format!("inline{port}"), json!(1));
            config.insert(format!("ratio{port}"), json!(1));
        }
        for (key, value) in [
            ("park_position", 10),
            ("park_move_speed", 20),
            ("park_move_timeout", 5),
            ("6dof_start_pos", 127),
            ("start_move_step", 30),
            ("start_move_timeout", 5),
            ("spike_move_speed", 24),
            ("spike_fine_move_speed", 11),
            ("klm_mode", 1),
            ("estop_mode", 2),
            ("force_offline_mode", 2),
            ("filter_factor", 0),
            ("platform_check", 1),
        ] {
            config.insert(key.into(), json!(value));
        }
        docs["status"]["devices"]["thanos"] =
            json!({"controller": controller, "firmware": "3.1", "config": config, "config2": config});
    }
    backend
}

#[test]
fn the_thanos_dialog_keeps_pending_settings_until_ok() {
    let backend = thanos_backend("Thanos4U(S)");
    let mut app = hardware(&backend);
    let model = crate::catalog::models().iter().find(|m| m.manufacturer == "THANOS").unwrap();
    choose_model(&mut app, &model.name).unwrap();
    group(&mut app, "Motion");
    assert!(main(&app).contains("data-arg=\"Hardware:motion_model:2\""));
    dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("Thanos - Thanos4U(S) - v3.1"), "the title names the controller and firmware");
    assert!(html.contains("Copy settings to all vertical actuators") && html.contains("Kill Motor Switch"));
    // The recommendation: 558 / 150 × 10 = 37°, 350 × 558 / 150 = 1302 (factor 1 for a Thanos4U).
    dispatch(&mut app, "dialog-field", "lateral", "558").unwrap();
    dispatch(&mut app, "dialog-field", "vertical", "150").unwrap();
    let Some(Device::Thanos(d)) = dh::device(&app).cloned() else { panic!("thanos dialog") };
    assert_eq!((d.max_angle, d.spike), (37, 1302));
    dispatch(&mut app, "dialog-field", "stroke:1", "200").unwrap();
    dispatch(&mut app, "dialog-field", "slider:park_position", "40").unwrap();
    dispatch(&mut app, "dialog-field", "mode:estop_mode", "0").unwrap();
    dispatch(&mut app, "dialog-field", "var:2:ratio", "2").unwrap();
    let Some(Device::Thanos(d)) = dh::device(&app).cloned() else { panic!() };
    assert!(d.pending.contains(&("stroke1".into(), 20)) && d.pending.contains(&("park_position".into(), 40)));
    assert!(d.pending2.contains(&("park_position".into(), 40)));
    assert!(d.pending.contains(&("estop_mode".into(), 1)) && d.pending.contains(&("ratio2".into(), 3)));
    // The calculator stores its values in PT-ACTUATOR.
    dispatch(&mut app, "dialog-field", "lateral", "600").unwrap();
    assert_eq!(native(&backend, "PT-ACTUATOR", "lateral_distance_actuators"), LegacyValue::Int(600));
    // Apply the max angle to every tune, after its confirmation.
    dispatch(&mut app, "dialog-button", "apply:angle", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("Max Angle Confirmation"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(
        called(&backend, Method::Post, "tunes/apply-all"),
        vec![Some(json!({"field": "max_angle", "value": 40}))]
    );
    assert!(matches!(dh::device(&app), Some(Device::Thanos(_))), "back in the controller dialog");
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let pending = called(&backend, Method::Put, "motion/thanos/pending");
    assert_eq!(pending[0].as_ref().unwrap()["replace"], json!(true));
    assert!(pending[0].as_ref().unwrap()["entries"].as_array().unwrap().contains(&json!(["stroke1", 20])));
    assert_eq!(
        pending.last().unwrap().as_ref().unwrap()["entries"],
        json!([["motor_number", model.motors as i64 - 4]])
    );
    assert!(app.ui.dialog.is_none());
    // The defaults: PT-Actuator strokes of 150 mm.
    dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
    dispatch(&mut app, "dialog-button", "defaults:P", "").unwrap();
    let Some(Device::Thanos(d)) = dh::device(&app).cloned() else { panic!() };
    assert!(d.pending.contains(&("stroke1".into(), 15)) && d.pending.contains(&("start_move_step".into(), 5)));
    drop(app.take_threads());
}

#[test]
fn the_pt_actuator_dialog_has_the_actuator_table_only() {
    let backend = thanos_backend("");
    let mut app = hardware(&backend);
    let model =
        crate::catalog::models().iter().find(|m| m.manufacturer == "PT-ACTUATOR" && m.name != "PTA Motion System");
    let Some(model) = model else { return };
    choose_model(&mut app, &model.name).unwrap();
    dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains(">PT-ACTUATOR<") && html.contains("Can ID") && !html.contains("MAX ANGLE"));
    drop(app.take_threads());
}

#[test]
fn the_pta_destinations_are_edited_and_stored() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    if crate::catalog::model("PTA Motion System").is_none() {
        return;
    }
    choose_model(&mut app, "PTA Motion System").unwrap();
    dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
    assert!(matches!(dh::device(&app), Some(Device::Pta { .. })));
    dispatch(&mut app, "dialog-field", "ip", "not an ip").unwrap();
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert!(crate::app::tests::alert_text(&app).unwrap().contains("is not a correctly formed IP address"));
    dispatch(&mut app, "dialog-field", "ip", "192.168.1.20").unwrap();
    dispatch(&mut app, "dialog-field", "port", "7408").unwrap();
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains(">192.168.1.20<"));
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert!(crate::app::tests::alert_text(&app).unwrap().contains("destination already"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(matches!(native(&backend, "PT-ACTUATOR", "udp_config"), LegacyValue::List(list) if !list.is_empty()));
    assert!(!called(&backend, Method::Post, "motion/disconnect").is_empty());
    drop(app.take_threads());
}

#[test]
fn dof_reality_vnm_and_yaw_vr_dialogs_store_their_settings() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    // DOF Reality: Auto Set KI and KP.
    dh::open_dof(&mut app).unwrap();
    let before = native_bool(&native(&backend, "DOF REALITY", "update_kp")).unwrap();
    dispatch(&mut app, "dialog-field", "update_kp", if before { "0" } else { "1" }).unwrap();
    assert_eq!(native_bool(&native(&backend, "DOF REALITY", "update_kp")).unwrap(), before, "stored on OK");
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(native(&backend, "DOF REALITY", "update_kp"), LegacyValue::Bool(!before));
    // VNM: the Reverse switches of ports 5–9.
    let vnm = crate::catalog::models()
        .iter()
        .filter(|m| m.manufacturer == "VNM SIMULATION")
        .max_by_key(|m| m.port_mapping.len())
        .unwrap();
    choose_model(&mut app, &vnm.name).unwrap();
    dh::open_vnm(&mut app).unwrap();
    if vnm.port_mapping.len() > 4 {
        assert!(crate::ui::render(&app, Region::Dialog).contains("data-arg=\"reverse:5\""));
    }
    dispatch(&mut app, "dialog-field", "reverse:5", "1").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(native(&backend, "VNM SIMULATION", "reverse_controller_port_5"), LegacyValue::Bool(true));
    // Yaw VR: Auto Park is stored at once, the limits on Confirm.
    backend.docs.lock().unwrap()["status"]["devices"]["yaw_vr"] = json!({
        "centered": false, "device_model": "YAWDEVICE",
        "limits": {"power": 17, "pitch_fwd_limit": 10, "pitch_bwd_limit": 11, "roll_limit": 12, "yaw_limit": 90,
                   "yaw_limit_enabled": 1, "vibration_enabled": 0}
    });
    dh::open_yaw_vr(&mut app).unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("max=\"35\""), "YAWDEVICE pitch limit");
    dispatch(&mut app, "dialog-field", "yaw:auto_park", "1").unwrap();
    assert_eq!(native(&backend, "Yaw VR", "auto_park"), LegacyValue::Bool(true));
    dispatch(&mut app, "dialog-field", "yaw:power", "60").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let limits = backend.patches("motion/yaw-vr/limits");
    assert!(limits.contains(&json!({"power": 60})), "{limits:?}");
    assert_eq!(native(&backend, "Yaw VR", "auto_park"), LegacyValue::Int(1));
    assert!(matches!(native(&backend, "Yaw VR", "6dof_mode"), LegacyValue::Int(_)));
    drop(app.take_threads());
}

#[test]
fn axis_assignment_sends_each_options_actuator() {
    let backend = FakeBackend::new();
    let model = crate::catalog::models().iter().find(|m| !m.assignment_options.is_empty()).unwrap();
    backend.docs.lock().unwrap()["status"]["motion"]["assignment"] = json!({"actuators": [
        {"key": 1, "axis": "1", "com_port": "/dev/ttyUSB0", "sn": "A1"},
        {"key": 2, "axis": "2", "com_port": "/dev/ttyUSB1", "sn": "B2"}
    ]});
    let mut app = hardware(&backend);
    choose_model(&mut app, &model.name).unwrap();
    dh::open_axis_assignment(&mut app).unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("Actuator 1 - Axis 1 - /dev/ttyUSB0 - S/N A1"));
    dispatch(&mut app, "dialog-field", "axis:0", "2").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let sent = called(&backend, Method::Post, "motion/axis-assignment");
    assert_eq!(sent[0].as_ref().unwrap()["assignment"]["0"], json!("2,B2"));
    drop(app.take_threads());
}

#[test]
fn the_magicbox_controller_asks_before_its_command() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    // The controller set-up is offered only outside the Windows rules, which the app always applies.
    assert!(!main(&app).contains("Hardware:controller:"));
    dispatch(&mut app, "action", "Hardware:controller:", "").unwrap();
    dispatch(&mut app, "dialog-field", "wheel", "AB12").unwrap();
    assert_eq!(app.setting(&["magicbox", "wheel_scan_code"]), &json!("AB12"));
    dispatch(&mut app, "dialog-button", "reboot", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("Reboot?"));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    assert!(matches!(dh::device(&app), Some(Device::Controller { confirm: None, .. })), "Cancel returns");
    dispatch(&mut app, "dialog-button", "shutdown", "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(app.ui.notices.history.iter().any(|n| n.message == "The shutdown has begun"));
    // The command runs on its own thread; the fixture does not start it.
    let threads = app.take_threads();
    assert_eq!(threads.len(), 1);
    drop(threads);
}

#[test]
fn start_opens_the_sound_device_and_sets_a_missing_model() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    backend.calls.lock().unwrap().clear();
    app.set_motion_setting_native_checked("model", &LegacyValue::Str("None".into())).unwrap();
    start(&mut app).unwrap();
    assert!(!called(&backend, Method::Put, "shakers/soundcard").is_empty());
    assert!(!called(&backend, Method::Post, "shakers/effects/reload").is_empty());
    drop(app.take_threads());
}

#[test]
fn every_hardware_text_has_an_entry_in_every_language() {
    let backend = thanos_backend("Thanos4U(S)");
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = hardware(&backend);
    backend.translations.lock().unwrap().clear();
    for theme in Theme::ALL {
        app.ui.theme = theme;
        for section in 0..9 {
            app.ui.section = section;
            main(&app);
        }
    }
    let thanos = crate::catalog::models().iter().find(|m| m.manufacturer == "THANOS").unwrap();
    choose_model(&mut app, &thanos.name).unwrap();
    let mut dialogs = vec![
        Device::Test(Test::Wind),
        Device::Test(Test::Speaker("FL".into())),
        Device::Test(Test::Haptic(1)),
        Device::Test(Test::Rumble(2)),
        Device::Duplicate { source: "x".into(), name: "x (1)".into(), error: Some("Enter a name.".into()) },
        Device::Duplicate {
            source: "x".into(),
            name: "x".into(),
            error: Some("A profile with this name exists already; choose another name.".into()),
        },
        Device::Remove { name: "x".into() },
        Device::Controller { wheel_id: String::new(), confirm: None },
        Device::Controller { wheel_id: String::new(), confirm: Some(true) },
        Device::Controller { wheel_id: String::new(), confirm: Some(false) },
        Device::Pta {
            ip: "1.2.3.4".into(),
            port: "7408".into(),
            list: LegacyValue::List(vec!["1.2.3.4:7408".into()]),
            attached: true,
        },
        Device::Dof { update_kp: true, changed: false },
    ];
    dh::open_thanos(&mut app).unwrap();
    let Some(Device::Thanos(d)) = dh::device(&app).cloned() else { panic!() };
    dialogs.push(Device::Thanos(d.clone()));
    for confirm in [true, false] {
        let mut d = d.clone();
        d.confirm = Some(confirm);
        dialogs.push(Device::Thanos(d));
    }
    app.set_setting_native_checked("SHAKEBOX", "soundcard", &LegacyValue::Str("USB Sound Device".into())).unwrap();
    dh::open_matrix(&mut app, "FL").unwrap();
    dialogs.push(dh::device(&app).cloned().unwrap());
    for dialog in dialogs {
        dh::open_device(&mut app, dialog);
        crate::ui::render(&app, Region::Dialog);
    }
    let looked_up: std::collections::BTreeSet<String> = backend.translations.lock().unwrap().iter().cloned().collect();
    let missing = crate::app::tests::texts_without_entry(&looked_up, &["USB Sound Device", "x", "x (1)", "FL"]);
    assert_eq!(missing, Vec::<String>::new());
    drop(app.take_threads());
}

// ---- Fixtures of the tests below -----------------------------------------------------------------------------

use crate::app::tests::{alert_text, dismiss_alerts, notice_texts};
use crate::backend::Failure;

/// Sets a value of the fake service's documents.
fn put(backend: &FakeBackend, path: &[&str], value: serde_json::Value) {
    crate::backend::set_at(&mut backend.docs.lock().unwrap(), path, value);
}

/// A value of the fake service's documents.
fn doc(backend: &FakeBackend, path: &[&str]) -> serde_json::Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs.clone(), |value, key| value[*key].clone())
}

/// The ids of the rows shown, in order.
fn rows_shown(html: &str) -> Vec<String> {
    html.split("data-control=\"").skip(1).map(|rest| rest.split('"').next().unwrap().to_owned()).collect()
}

/// The markup of one row, up to the next row.
fn row_of<'a>(html: &'a str, id: &str) -> &'a str {
    let marker = format!("data-control=\"{id}\"");
    let start = html.find(&marker).unwrap_or_else(|| panic!("row {id} in {html}"));
    let rest = &html[start + marker.len()..];
    let end = rest.find("data-control=\"").unwrap_or(rest.len());
    &html[start..start + marker.len() + end]
}

/// The tag of the element with this id.
fn tag<'a>(html: &'a str, id: &str) -> &'a str {
    let at = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("{id} in {html}"));
    let start = html[..at].rfind('<').unwrap();
    let end = at + html[at..].find('>').unwrap();
    &html[start..=end]
}

/// The texts of a select's options and the selected one.
fn options_of(html: &str, id: &str) -> (Vec<String>, Option<String>) {
    let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("{id} in {html}"));
    let select = &html[start..start + html[start..].find("</select>").unwrap()];
    let mut texts = Vec::new();
    let mut selected = None;
    for option in select.split("<option ").skip(1) {
        let (attributes, rest) = option.split_once('>').unwrap();
        let shown = rest.split("</option>").next().unwrap().to_owned();
        if attributes.contains(" disabled") {
            continue;
        }
        if attributes.contains(" selected") {
            selected = Some(shown.clone());
        }
        texts.push(shown);
    }
    (texts, selected)
}

/// Whether the button with this action argument is enabled (it must be shown).
fn button_enabled(html: &str, arg: &str) -> bool {
    if html.contains(&format!("data-arg=\"{arg}\">")) {
        true
    } else if html.contains(&format!("data-arg=\"{arg}\" disabled>")) {
        false
    } else {
        panic!("no button {arg} in {html}")
    }
}

/// The value a slider row of a settings group shows.
fn shown_value(app: &MyApp, title: &str, id: &str) -> f64 {
    let index = spec::hardware_page().sections.iter().position(|s| s.title == title).unwrap();
    let html = main_of(app, index);
    let tag = tag(&html, &format!("c-hardware-{id}"));
    let value = tag.split("value=\"").nth(1).unwrap().split('"').next().unwrap();
    value.parse().unwrap()
}

/// The body drawn for a settings group.
fn main_of(app: &MyApp, section: usize) -> String {
    let rules = Rules::read(app);
    let page = page(app, &rules);
    let disabled = disabled_buttons(&rules);
    page.sections[section]
        .controls
        .iter()
        .filter_map(|control| {
            row_state(app, &rules, control).map(|state| disable(&row(app, control, &state), &disabled))
        })
        .collect()
}

/// A service whose answers to one route fail; everything else goes to the fake service.
struct Failing {
    inner: std::sync::Arc<FakeBackend>,
    path: &'static str,
    detail: String,
}

impl Backend for Failing {
    fn call(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, Failure> {
        if path == self.path {
            self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
            return Err(Failure { status: 500, detail: self.detail.clone() });
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        self.inner.native_setting(section, key)
    }
    fn hardware_parameter(&self, name: &str) -> Result<LegacyValue, Failure> {
        self.inner.hardware_parameter(name)
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<serde_json::Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        self.inner.excluded_strings()
    }
}

fn first_model(filter: impl Fn(&ModelFlags) -> bool) -> &'static ModelFlags {
    crate::catalog::models().iter().find(|m| filter(m)).expect("a catalog model")
}

fn largest_model(manufacturer: &str) -> &'static ModelFlags {
    crate::catalog::models()
        .iter()
        .filter(|m| m.manufacturer == manufacturer)
        .max_by_key(|m| m.port_mapping.len())
        .expect("a catalog model")
}

// ---- Shakers: layouts, the sample switch and the SRS Installer row ----------------------------------------------

#[test]
fn the_delete_question_names_the_chosen_layout_and_cancel_deletes_nothing() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["layouts"] = json!([
            {"name": "Seat", "builtin": false, "speakers": {"FL": true}},
            {"name": "Pedals", "builtin": false, "speakers": {"FL": true}}
        ]);
        docs["settings"]["shakers"]["speaker_layout"] = json!("Seat");
        docs["settings"]["shakers"]["sound_card"] = json!("USB Sound Device");
    }
    let mut app = hardware(&backend);
    group(&mut app, "Shakers");
    backend.calls.lock().unwrap().clear();
    for name in ["Seat", "Pedals", "Seat"] {
        let index = option_index(&main(&app), "c-hardware-shakers_profile", name);
        dispatch(&mut app, "set", "Hardware:shakers_profile", &index).unwrap();
        dispatch(&mut app, "action", "Hardware:shakers_profile:1", "").unwrap();
        let html = crate::ui::render(&app, Region::Dialog);
        assert!(html.contains(&format!("Please confirm that this profile should be deleted: {name}")), "{html}");
        assert_eq!(dh::device(&app), Some(&Device::Remove { name: name.into() }));
        dispatch(&mut app, "dialog-close", "", "").unwrap();
        assert!(app.ui.dialog.is_none());
    }
    assert!(called(&backend, Method::Post, "shakers/rows/delete").is_empty());
    assert_eq!(native(&backend, "SHAKEBOX", "profile"), LegacyValue::Str("Seat".into()));
    drop(app.take_threads());
}

#[test]
fn a_refused_copy_name_keeps_the_layout_and_copies_nothing() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    group(&mut app, "Shakers");
    let source = setting_text(&app, "SHAKEBOX", "profile");
    dispatch(&mut app, "action", "Hardware:shakers_profile:0", "").unwrap();
    dispatch(&mut app, "dialog-field", "name", &source).unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog)
        .contains("A profile with this name exists already; choose another name."));
    assert!(matches!(dh::device(&app), Some(Device::Duplicate { error: Some(_), .. })), "the dialog stays open");
    assert!(called(&backend, Method::Post, "shakers/rows/copy").is_empty());
    assert_eq!(setting_text(&app, "SHAKEBOX", "profile"), source);
    drop(app.take_threads());
}

/// A copy's name is taken as typed: spaces are kept and an empty name is not refused.
#[test]
fn a_copy_name_is_taken_as_typed() {
    for name in ["  Seat copy  ", ""] {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        group(&mut app, "Shakers");
        let source = setting_text(&app, "SHAKEBOX", "profile");
        dispatch(&mut app, "action", "Hardware:shakers_profile:0", "").unwrap();
        dispatch(&mut app, "dialog-field", "name", name).unwrap();
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert!(dh::device(&app).is_none(), "{name:?}: copied, the dialog closes");
        let copies = called(&backend, Method::Post, "shakers/rows/copy");
        assert_eq!(copies.len(), 1, "{name:?}");
        assert_eq!(copies[0].as_ref().unwrap()["name"], json!(name), "{name:?}: {copies:?}");
        assert_eq!(copies[0].as_ref().unwrap()["source"], json!(source), "{copies:?}");
        drop(app.take_threads());
    }
}

#[test]
fn the_sample_switch_shows_the_stored_choice_and_follows_outside_changes() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let html = group(&mut app, "Shakers");
    let row = row_of(&html, "use_srs_sounds");
    assert!(!tag(row, "c-hardware-use_srs_sounds").contains(" checked"), "the built-in sounds by default");
    assert!(row.contains("SRS Installer row below") && !row.contains("--srs-install"), "{row}");
    for enabled in [true, false, true] {
        put(&backend, &["settings", "shakers", "use_srs_sounds"], json!(enabled));
        app.refresh_docs();
        let html = main(&app);
        assert_eq!(tag(&html, "c-hardware-use_srs_sounds").contains(" checked"), enabled);
    }
    drop(app.take_threads());
}

#[test]
fn the_sample_switch_writes_only_the_sample_choice() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    group(&mut app, "Shakers");
    let tune = doc(&backend, &["tune"]);
    backend.calls.lock().unwrap().clear();
    for enabled in [true, false] {
        dispatch(&mut app, "set", "Hardware:use_srs_sounds", if enabled { "1" } else { "0" }).unwrap();
        assert_eq!(app.setting(&["shakers", "use_srs_sounds"]), &json!(enabled));
        assert_eq!(doc(&backend, &["settings", "shakers", "use_srs_sounds"]), json!(enabled));
        assert_eq!(tag(&main(&app), "c-hardware-use_srs_sounds").contains(" checked"), enabled);
    }
    assert_eq!(
        backend.patches("settings"),
        [json!({"shakers": {"use_srs_sounds": true}}), json!({"shakers": {"use_srs_sounds": false}})]
    );
    assert!(called(&backend, Method::Put, "shakers/soundcard").is_empty());
    assert!(called(&backend, Method::Patch, "profile").is_empty());
    assert_eq!(doc(&backend, &["tune"]), tune);
    drop(app.take_threads());
}

#[test]
fn the_sample_switch_reads_yes_and_no_texts() {
    let backend = FakeBackend::new();
    put(&backend, &["settings", "shakers", "use_srs_sounds"], json!("yes"));
    let mut app = hardware(&backend);
    let html = group(&mut app, "Shakers");
    assert!(tag(&html, "c-hardware-use_srs_sounds").contains(" checked"), "a stored yes is on");
    put(&backend, &["settings_failure"], json!("write rejected"));
    dispatch(&mut app, "set", "Hardware:use_srs_sounds", "0").unwrap();
    assert!(tag(&main(&app), "c-hardware-use_srs_sounds").contains(" checked"), "a refused change keeps yes");
    put(&backend, &["settings_failure"], serde_json::Value::Null);
    put(&backend, &["settings", "shakers", "use_srs_sounds"], json!("no"));
    app.refresh_docs();
    assert!(!tag(&main(&app), "c-hardware-use_srs_sounds").contains(" checked"), "a stored no is off");
    drop(app.take_threads());
}

#[test]
fn a_refused_sample_choice_keeps_the_stored_one_and_shows_the_reason() {
    let backend = FakeBackend::new();
    put(&backend, &["settings", "shakers", "use_srs_sounds"], json!(false));
    let mut app = hardware(&backend);
    group(&mut app, "Shakers");
    for message in ["sample choice failed", "sample choice failed again"] {
        put(&backend, &["settings_failure"], json!(message));
        dispatch(&mut app, "set", "Hardware:use_srs_sounds", "1").unwrap();
        assert_eq!(alert_text(&app).as_deref(), Some(message));
        assert_eq!(app.setting(&["shakers", "use_srs_sounds"]), &json!(false));
        assert_eq!(doc(&backend, &["settings", "shakers", "use_srs_sounds"]), json!(false));
        assert!(!tag(&main(&app), "c-hardware-use_srs_sounds").contains(" checked"));
    }
    put(&backend, &["settings_failure"], serde_json::Value::Null);
    dispatch(&mut app, "set", "Hardware:use_srs_sounds", "1").unwrap();
    assert_eq!(doc(&backend, &["settings", "shakers", "use_srs_sounds"]), json!(true));
    assert!(tag(&main(&app), "c-hardware-use_srs_sounds").contains(" checked"));
    drop(app.take_threads());
}

#[test]
fn the_srs_import_sends_the_typed_path_and_reports_the_outcome() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let html = group(&mut app, "Shakers");
    let row = row_of(&html, "srs_import");
    assert!(row.contains("<strong>SRS Installer</strong>") && row.contains(">Import</button>"));
    assert!(row.contains("placeholder=\"Path of the SRS installer (.exe) or installation folder\""));
    backend.calls.lock().unwrap().clear();
    for blank in ["", "   "] {
        dispatch(&mut app, "set", "Hardware:srs_path", blank).unwrap();
        dispatch(&mut app, "action", "Hardware:srs_import:", "").unwrap();
        assert_eq!(
            alert_text(&app).as_deref(),
            Some("Enter the path of the SRS installer (.exe) or installation folder.")
        );
        dismiss_alerts(&mut app);
    }
    assert!(called(&backend, Method::Post, "srs-import").is_empty(), "nothing is imported without a path");
    dispatch(&mut app, "set", "Hardware:srs_path", " ~/Downloads/SRS Setup.exe ").unwrap();
    app.backend = std::sync::Arc::new(Failing {
        inner: backend.clone(),
        path: "srs-import",
        detail: "/home/user/Downloads/SRS Setup.exe: No such file or directory".into(),
    });
    dispatch(&mut app, "action", "Hardware:srs_import:", "").unwrap();
    assert_eq!(
        notice_texts(&app).last().cloned(),
        Some((
            "SRS Installer".to_owned(),
            "Could not import the SRS files: /home/user/Downloads/SRS Setup.exe: No such file or directory".to_owned()
        ))
    );
    app.backend = backend.clone();
    dispatch(&mut app, "action", "Hardware:srs_import:", "").unwrap();
    let (title, shown) = notice_texts(&app).last().cloned().unwrap();
    assert_eq!(title, "SRS Installer");
    assert!(shown.starts_with("The SRS files were imported.") && shown.contains("next start"), "{shown}");
    assert!(main(&app).contains("value=\" ~/Downloads/SRS Setup.exe \""), "the typed path stays in the field");
    assert_eq!(
        called(&backend, Method::Post, "srs-import"),
        [Some(json!({"path": "~/Downloads/SRS Setup.exe"})), Some(json!({"path": "~/Downloads/SRS Setup.exe"}))]
    );
    assert!(backend.patches("settings").is_empty(), "the import does not change the sample switch");
    drop(app.take_threads());
}

// ---- The groups and their rows -----------------------------------------------------------------------------------

#[test]
fn the_groups_follow_the_supported_hardware_order() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = json!(true);
    let mut app = hardware(&backend);
    let html = main(&app);
    let titles = [
        "Intellibox",
        "Shakers",
        "Motion",
        "Motion Compensation (WITMotion)",
        "Belt",
        "SIM3D",
        "HUD",
        "IFlag",
        "HapticR",
    ];
    let positions: Vec<usize> =
        titles.iter().map(|t| html.find(&format!(">{t}</option>")).unwrap_or_else(|| panic!("{t}"))).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{positions:?}");
    let mut expected = vec!["auto_search_HAPTICR"];
    if cfg!(windows) {
        expected.push("com_port_HAPTICR");
    }
    expected.extend(["haptic_test_front", "haptic_test_rear"]);
    assert_eq!(rows_shown(&group(&mut app, "HapticR")), expected);
    drop(app.take_threads());
}

#[test]
fn each_group_shows_its_rows_in_order_with_the_lists_filled() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["hardware"]["sound_cards"] = json!(["None", "Unknown", "USB Sound Device"]);
        docs["layouts"] = json!([
            {"name": "SRS UShake6", "builtin": true, "speakers": {"FL": true}},
            {"name": "My Layout", "builtin": false, "speakers": {"FL": true}}
        ]);
        docs["settings"]["shakers"]["speaker_layout"] = json!("SRS UShake6");
        docs["settings"]["sim3d"]["model"] = json!("4 Ports");
    }
    let mut app = hardware(&backend);
    // The Com Port rows name Windows ports: only shown where this program runs on Windows.
    let port = |key: &str| if cfg!(windows) { vec![format!("com_port_{key}")] } else { Vec::new() };
    let rows = |app: &mut MyApp, title: &str| rows_shown(&group(app, title));
    let mut intellibox = vec!["auto_search_INTELLIBOX".to_owned()];
    intellibox.extend(port("INTELLIBOX"));
    intellibox.extend(
        ["wind_type", "nbr_fans", "led_model", "nbr_leds", "brightness", "shift_light_dim_perc"].map(String::from),
    );
    assert_eq!(rows(&mut app, "Intellibox"), intellibox);
    assert_eq!(
        rows(&mut app, "Shakers"),
        ["soundcard", "shakers_profile", "fl", "fr", "ce", "sub", "rl", "rr", "use_srs_sounds", "srs_import"]
    );
    let mut motion =
        ["auto_search_MOTIONBOX", "motion_manufacturer", "motion_model", "auto_smoothing_low_speed", "motion_test"]
            .map(String::from)
            .to_vec();
    motion.extend(["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss"].map(String::from));
    assert_eq!(rows(&mut app, "Motion"), motion);
    let mut mc = vec!["auto_search_MOTION_COMPENSATION".to_owned()];
    mc.extend(port("MOTION_COMPENSATION"));
    assert_eq!(rows(&mut app, "Motion Compensation (WITMotion)"), mc);
    let mut belt = vec!["auto_search_BELT_TENSIONER".to_owned()];
    belt.extend(port("BELT_TENSIONER"));
    belt.extend(
        ["belt_test", "belt_test_slider", "belt_calibration_power_slider", "belt_calibration_offset_slider"]
            .map(String::from),
    );
    assert_eq!(rows(&mut app, "Belt"), belt);
    let mut sim3d = vec!["auto_search_SIM3D".to_owned()];
    sim3d.extend(port("SIM3D"));
    sim3d.extend(["rumble_model", "rumble_port1", "rumble_port2", "rumble_port3", "rumble_port4"].map(String::from));
    assert_eq!(rows(&mut app, "SIM3D"), sim3d);
    assert_eq!(rows(&mut app, "HUD"), ["auto_search_HUD", "hud_model"]);
    let mut iflag = vec!["auto_search_IFLAG".to_owned()];
    iflag.extend(port("IFLAG"));
    iflag.extend(["iflag_dim_perc", "iflag_mirror", "iflag_orientation", "iflag_test"].map(String::from));
    assert_eq!(rows(&mut app, "IFlag"), iflag);
    // The Com Port list: Auto Search (stored as an empty text) and COM1…COM99.
    let ports = com_ports();
    assert_eq!(ports.len(), 100);
    assert_eq!((ports[0].label.as_str(), &ports[0].value), ("Auto Search", &json!("")));
    assert_eq!((ports[99].label.as_str(), &ports[99].value), ("COM99", &json!("COM99")));
    // The sound devices without `Unknown`; the speaker layouts sorted.
    let html = group(&mut app, "Shakers");
    assert_eq!(options_of(&html, "c-hardware-soundcard").0, ["None", "USB Sound Device"]);
    assert_eq!(options_of(&html, "c-hardware-shakers_profile").0, ["My Layout", "SRS UShake6"]);
    drop(app.take_threads());
}

// ---- The controller dialogs: VNM, Thanos reverse, DOF Reality, Yaw VR -------------------------------------------

fn vnm_backend() -> std::sync::Arc<FakeBackend> {
    let backend = FakeBackend::new();
    for port in 5..10 {
        put(&backend, &["settings", "vnm", &format!("reverse_controller_port_{port}")], json!(true));
    }
    backend
}

#[test]
fn the_vnm_dialog_reads_the_current_flags_and_ports() {
    let backend = vnm_backend();
    let mut app = hardware(&backend);
    let vnm = largest_model("VNM SIMULATION");
    assert!(vnm.port_mapping.len() > 4);
    choose_model(&mut app, &vnm.name).unwrap();
    put(&backend, &["settings", "vnm", "reverse_controller_port_5"], json!("no"));
    put(&backend, &["status", "motion", "link", "ports"], json!({"1": "COM7", "2": "COM8"}));
    dispatch(&mut app, "action", "Hardware:motion_model:4", "").unwrap();
    let Some(Device::Vnm { actuators, reverse }) = dh::device(&app).cloned() else { panic!("VNM dialog") };
    assert_eq!(reverse[..2], [(5, false), (6, true)]);
    assert_eq!(actuators[0].port, "Main - 1");
    assert_eq!(actuators[4].port, "Secondary - 1");
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("Main - 1") && html.contains("Secondary - 1"));
    assert!(
        !tag(&html, "thanos-reverse-5").contains(" checked") && tag(&html, "thanos-reverse-6").contains(" checked")
    );
    drop(app.take_threads());
}

#[test]
fn an_unreadable_vnm_flag_or_port_list_keeps_the_dialog_closed() {
    let backend = vnm_backend();
    let mut app = hardware(&backend);
    choose_model(&mut app, &largest_model("VNM SIMULATION").name).unwrap();
    put(&backend, &["settings", "vnm", "reverse_controller_port_7"], json!("broken"));
    assert_eq!(
        dispatch(&mut app, "action", "Hardware:motion_model:4", ""),
        Err("Exception: Boolean value expected. Got broken".into())
    );
    assert!(app.ui.dialog.is_none());
    put(&backend, &["settings", "vnm", "reverse_controller_port_7"], json!(true));
    put(&backend, &["status", "motion", "link", "ports"], serde_json::Value::Null);
    assert_eq!(
        dispatch(&mut app, "action", "Hardware:motion_model:4", ""),
        Err("TypeError: object of type 'NoneType' has no len()".into())
    );
    assert!(app.ui.dialog.is_none());
    // A model the catalog does not know: the flags are read, then the dialog stays closed.
    app.set_motion_setting_native_checked("model", &LegacyValue::Str("Unknown".into())).unwrap();
    assert_eq!(dh::open_vnm(&mut app), Err("KeyError: 'Unknown'".into()));
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn the_vnm_reverse_switches_are_stored_on_ok_only() {
    let backend = vnm_backend();
    let mut app = hardware(&backend);
    choose_model(&mut app, &largest_model("VNM SIMULATION").name).unwrap();
    dh::open_vnm(&mut app).unwrap();
    dispatch(&mut app, "dialog-field", "reverse:5", "0").unwrap();
    assert_eq!(native(&backend, "VNM SIMULATION", "reverse_controller_port_5"), LegacyValue::Bool(true));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(native(&backend, "VNM SIMULATION", "reverse_controller_port_5"), LegacyValue::Bool(false));
    assert_eq!(native(&backend, "VNM SIMULATION", "reverse_controller_port_6"), LegacyValue::Bool(true));
    // Without the VNM settings section the confirmation fails and stores nothing.
    dh::open_vnm(&mut app).unwrap();
    put(&backend, &["settings", "vnm"], serde_json::Value::Null);
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert_eq!(doc(&backend, &["settings", "vnm"]), serde_json::Value::Null);
    drop(app.take_threads());
}

#[test]
fn a_small_vnm_table_does_not_read_the_ports() {
    let backend = vnm_backend();
    let mut app = hardware(&backend);
    let model = first_model(|m| m.manufacturer == "VNM SIMULATION" && m.port_mapping.len() <= 4);
    choose_model(&mut app, &model.name).unwrap();
    put(&backend, &["status", "motion", "link", "ports"], serde_json::Value::Null);
    dh::open_vnm(&mut app).unwrap();
    let Some(Device::Vnm { actuators, .. }) = dh::device(&app).cloned() else { panic!("VNM dialog") };
    assert_eq!(actuators.len(), model.port_mapping.len());
    assert!(actuators.iter().all(|a| a.reverse.is_none()));
    assert_eq!(actuators[0].port, "1");
    drop(app.take_threads());
}

#[test]
fn the_thanos_reverse_switches_keep_their_order_until_ok() {
    let backend = thanos_backend("");
    put(&backend, &["settings", "thanos", "reverse_controller_port_8"], json!(true));
    let mut app = hardware(&backend);
    let model = largest_model("THANOS");
    choose_model(&mut app, &model.name).unwrap();
    dispatch(&mut app, "action", "Hardware:motion_model:2", "").unwrap();
    let Some(Device::Thanos(before)) = dh::device(&app).cloned() else { panic!("thanos dialog") };
    assert_eq!(before.reverse, [(5, false), (6, false), (7, false), (8, true)]);
    dispatch(&mut app, "dialog-field", "reverse:5", "1").unwrap();
    let Some(Device::Thanos(after)) = dh::device(&app).cloned() else { panic!("thanos dialog") };
    assert_eq!(after.reverse, [(5, true), (6, false), (7, false), (8, true)]);
    assert_eq!(native(&backend, "THANOS", "reverse_controller_port_5"), LegacyValue::Bool(false), "kept until OK");
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(native(&backend, "THANOS", "reverse_controller_port_5"), LegacyValue::Bool(true));
    assert_eq!(native(&backend, "THANOS", "reverse_controller_port_8"), LegacyValue::Bool(true));
    drop(app.take_threads());
}

#[test]
fn the_dof_reality_dialog_reads_the_current_flag_and_needs_its_section() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    put(&backend, &["settings", "dof_reality", "update_kp"], json!("no"));
    dh::open_dof(&mut app).unwrap();
    assert_eq!(dh::device(&app), Some(&Device::Dof { update_kp: false, changed: false }));
    assert!(!tag(&crate::ui::render(&app, Region::Dialog), "dof-update-kp").contains(" checked"));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    put(&backend, &["settings", "dof_reality", "update_kp"], json!("broken"));
    assert_eq!(dh::open_dof(&mut app), Err("Exception: Boolean value expected. Got broken".into()));
    assert!(app.ui.dialog.is_none());
    // A confirmation without the DOF Reality section fails and stores nothing.
    put(&backend, &["settings", "dof_reality", "update_kp"], json!(true));
    dh::open_dof(&mut app).unwrap();
    dispatch(&mut app, "dialog-field", "update_kp", "0").unwrap();
    put(&backend, &["settings", "dof_reality"], serde_json::Value::Null);
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert_eq!(doc(&backend, &["settings", "dof_reality"]), serde_json::Value::Null);
    drop(app.take_threads());
}

fn yaw_backend(power: serde_json::Value) -> std::sync::Arc<FakeBackend> {
    let backend = FakeBackend::new();
    put(
        &backend,
        &["status", "devices", "yaw_vr"],
        json!({"centered": false, "device_model": "", "limits": {
            "power": power, "pitch_fwd_limit": 10, "pitch_bwd_limit": 10, "roll_limit": 10,
            "yaw_limit": 90, "yaw_limit_enabled": 1, "vibration_enabled": 1
        }}),
    );
    for (key, value) in
        [("vibration_power", json!(25)), ("auto_park", json!(false)), ("center_before_start_alert", json!(false))]
    {
        put(&backend, &["settings", "yaw_vr", key], value);
    }
    put(&backend, &["settings", "yaw_vr", "six_dof_mode"], json!(true));
    backend
}

fn yaw_value(app: &MyApp, key: &str) -> dh::YawField {
    let Some(Device::YawVr { fields }) = dh::device(app) else { panic!("Yaw VR dialog") };
    fields.iter().find(|(k, ..)| *k == key).unwrap().2.clone()
}

#[test]
fn the_yaw_vr_dialog_reads_the_current_values() {
    let backend = yaw_backend(json!(12));
    let mut app = hardware(&backend);
    put(&backend, &["status", "devices", "yaw_vr", "limits", "power"], json!(17));
    put(&backend, &["settings", "yaw_vr", "auto_park"], json!("False"));
    dh::open_yaw_vr(&mut app).unwrap();
    assert_eq!(yaw_value(&app, "power"), dh::YawField::Slider { max: 100, value: 17, shown: "17".into() });
    assert_eq!(yaw_value(&app, "auto_park"), dh::YawField::Switch(false), "a stored False text is off");
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(tag(&html, "yaw-power").contains("value=\"17\""));
    assert!(html.contains("<output id=\"yaw-power-out\" for=\"yaw-power\">17</output>"));
    drop(app.take_threads());
}

#[test]
fn the_yaw_vr_limits_keep_their_range_and_show_the_platforms_value() {
    let backend = yaw_backend(json!(12));
    let mut app = hardware(&backend);
    put(&backend, &["status", "devices", "yaw_vr", "limits", "power"], json!(-3));
    put(&backend, &["status", "devices", "yaw_vr", "limits", "yaw_limit"], json!(17.5));
    put(&backend, &["status", "devices", "yaw_vr", "limits", "roll_limit"], json!(500));
    dh::open_yaw_vr(&mut app).unwrap();
    assert_eq!(yaw_value(&app, "power"), dh::YawField::Slider { max: 100, value: 0, shown: "-3".into() });
    // A fraction is dropped toward zero, not rounded.
    assert_eq!(yaw_value(&app, "yaw_limit"), dh::YawField::Slider { max: 180, value: 17, shown: "17.5".into() });
    let dh::YawField::Slider { max, value, .. } = yaw_value(&app, "roll_limit") else { panic!("a slider") };
    assert_eq!(value, max, "kept at the top of its range");
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("<output id=\"yaw-yaw_limit-out\" for=\"yaw-yaw_limit\">17.5</output>"), "{html}");
    // A moved slider shows its own value and stays within its range.
    dispatch(&mut app, "dialog-field", "yaw:power", "140").unwrap();
    assert_eq!(yaw_value(&app, "power"), dh::YawField::Slider { max: 100, value: 100, shown: "100".into() });
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let limits = backend.patches("motion/yaw-vr/limits");
    assert!(limits.iter().any(|patch| patch["power"] == json!(100)), "{limits:?}");
    assert!(limits.iter().any(|patch| patch["roll_limit"] == json!(max)), "{limits:?}");
    // A limit the platform does not report as a number keeps the dialog closed.
    put(&backend, &["status", "devices", "yaw_vr", "limits", "power"], serde_json::Value::Null);
    let error = dh::open_yaw_vr(&mut app).unwrap_err();
    assert!(error.starts_with("TypeError: '>' not supported"), "{error}");
    drop(app.take_threads());
}

#[test]
fn the_yaw_vr_dialog_stays_closed_without_limits_or_with_an_unreadable_flag() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let error = dh::open_yaw_vr(&mut app).unwrap_err();
    assert!(error.starts_with("KeyError:"), "{error}");
    assert!(app.ui.dialog.is_none());
    let backend = yaw_backend(json!(12));
    let mut app = hardware(&backend);
    put(&backend, &["settings", "yaw_vr", "auto_park"], json!("broken"));
    assert_eq!(dh::open_yaw_vr(&mut app), Err("Exception: Boolean value expected. Got broken".into()));
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn the_yaw_vr_confirmation_writes_the_limits_in_order_and_integer_settings() {
    let backend = yaw_backend(json!(12));
    let mut app = hardware(&backend);
    dh::open_yaw_vr(&mut app).unwrap();
    dispatch(&mut app, "dialog-field", "yaw:power", "17").unwrap();
    // A value that is no number is refused and leaves the shown value.
    for bad in ["NaN", "inf", "x"] {
        assert!(dispatch(&mut app, "dialog-field", "yaw:pitch_fwd_limit", bad).unwrap_err().starts_with("ValueError"));
    }
    assert_eq!(yaw_value(&app, "pitch_fwd_limit"), dh::YawField::Slider { max: 15, value: 10, shown: "10".into() });
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let limits = called(&backend, Method::Patch, "motion/yaw-vr/limits");
    assert_eq!(limits[0], Some(json!({"power": 17})));
    assert_eq!(limits[1], Some(json!({"pitch_fwd_limit": 10})));
    assert_eq!(native(&backend, "Yaw VR", "vibration_power"), LegacyValue::Int(25));
    assert_eq!(native(&backend, "Yaw VR", "auto_park"), LegacyValue::Int(0));
    assert_eq!(native(&backend, "Yaw VR", "center_before_start_alert"), LegacyValue::Int(0));
    assert_eq!(native(&backend, "Yaw VR", "6dof_mode"), LegacyValue::Int(1));
    drop(app.take_threads());
}

#[test]
fn a_fractional_yaw_limit_is_confirmed_as_its_whole_part() {
    let backend = yaw_backend(json!(12));
    let mut app = hardware(&backend);
    put(&backend, &["status", "devices", "yaw_vr", "limits", "yaw_limit"], json!(12.5));
    dh::open_yaw_vr(&mut app).unwrap();
    assert_eq!(yaw_value(&app, "yaw_limit"), dh::YawField::Slider { max: 180, value: 12, shown: "12.5".into() });
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let limits = called(&backend, Method::Patch, "motion/yaw-vr/limits");
    assert!(limits.contains(&Some(json!({"yaw_limit": 12}))), "{limits:?}");
    drop(app.take_threads());
}

#[test]
fn auto_park_and_the_centre_alert_are_stored_at_once() {
    for key in ["auto_park", "center_before_start_alert"] {
        let backend = yaw_backend(json!(12));
        let mut app = hardware(&backend);
        dh::open_yaw_vr(&mut app).unwrap();
        dispatch(&mut app, "dialog-field", &format!("yaw:{key}"), "1").unwrap();
        assert_eq!(native(&backend, "Yaw VR", key), LegacyValue::Bool(true), "{key}");
        dispatch(&mut app, "dialog-field", &format!("yaw:{key}"), "0").unwrap();
        assert_eq!(native(&backend, "Yaw VR", key), LegacyValue::Bool(false), "{key}");
        assert!(matches!(dh::device(&app), Some(Device::YawVr { .. })), "the dialog stays open");
        drop(app.take_threads());
    }
}

#[test]
fn limit_yaw_enables_the_yaw_slider() {
    let backend = yaw_backend(json!(12));
    let mut app = hardware(&backend);
    dh::open_yaw_vr(&mut app).unwrap();
    let yaw_disabled =
        |app: &MyApp| tag(&crate::ui::render(app, Region::Dialog), "yaw-yaw_limit").contains(" disabled");
    assert!(!yaw_disabled(&app));
    dispatch(&mut app, "dialog-field", "yaw:yaw_limit_enabled", "0").unwrap();
    assert!(yaw_disabled(&app));
    dispatch(&mut app, "dialog-field", "yaw:yaw_limit_enabled", "1").unwrap();
    assert!(!yaw_disabled(&app));
    drop(app.take_threads());
}

// ---- The belt and motion test sliders and buttons ----------------------------------------------------------------

#[test]
fn the_test_sliders_refuse_values_that_are_no_number() {
    for (slider, path) in
        [("belt_test_slider", vec!["tests", "belt_test_value"]), ("pitch", vec!["tests", "motion_sliders", "pitch"])]
    {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        let mut full = vec!["status", "controls"];
        full.extend(&path);
        put(&backend, &full, json!(13));
        backend.calls.lock().unwrap().clear();
        for bad in ["NaN", "inf", "-inf", "x"] {
            let error = dispatch(&mut app, "set", &format!("Hardware:{slider}"), bad).unwrap_err();
            assert!(error.starts_with("ValueError"), "{slider} {bad}: {error}");
        }
        assert_eq!(control(&backend, &path), json!(13));
        assert!(backend.patches("controls").is_empty(), "{slider}");
        drop(app.take_threads());
    }
}

#[test]
fn the_test_sliders_keep_whole_numbers_whole() {
    for (slider, path) in
        [("belt_test_slider", vec!["tests", "belt_test_value"]), ("pitch", vec!["tests", "motion_sliders", "pitch"])]
    {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        dispatch(&mut app, "set", &format!("Hardware:{slider}"), "17").unwrap();
        let stored = control(&backend, &path);
        assert!(stored.is_i64() && stored == json!(17), "{slider}: {stored}");
        dispatch(&mut app, "set", &format!("Hardware:{slider}"), "17.5").unwrap();
        assert_eq!(control(&backend, &path), json!(17.5));
        assert_eq!(app.control(&path), &json!(17.5), "the screen shows what the service holds");
        drop(app.take_threads());
    }
}

#[test]
fn a_refused_test_slider_write_is_reported_and_changes_nothing() {
    for (slider, path) in
        [("belt_test_slider", vec!["tests", "belt_test_value"]), ("pitch", vec!["tests", "motion_sliders", "pitch"])]
    {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        let mut full = vec!["status", "controls"];
        full.extend(&path);
        put(&backend, &full, json!(13));
        put(&backend, &["control_failure"], json!("RuntimeError: scalar write failed"));
        assert_eq!(
            dispatch(&mut app, "set", &format!("Hardware:{slider}"), "17"),
            Err("RuntimeError: scalar write failed".into())
        );
        assert_eq!(control(&backend, &path), json!(13));
        drop(app.take_threads());
    }
}

#[test]
fn the_actuator_positions_of_a_measurement_row() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    put(&backend, &["status", "motion", "actuators"], json!({"motors": [3, -0.0, null, "x", [1, 2], false]}));
    assert_eq!(motor_positions(&app), ["3", "-0.0", "None", "x", "[1, 2]", "False"]);
    put(&backend, &["status", "motion", "actuators"], json!({"m0": 17, "m2": null}));
    assert_eq!(motor_positions(&app), ["17", "0", "None", "0", "0", "0"]);
    drop(app.take_threads());
}

#[test]
fn the_measurement_shows_its_progress_and_stops_on_request() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    app.ui.devices.hardware.sweep =
        Some(Sweep { row: 3, total: 1331, path: "/data/data_20260101_120000.csv".into(), stop: stop.clone() });
    let html = group(&mut app, "Motion");
    assert!(html.contains("Measuring 3 / 1331") && html.contains("/data/data_20260101_120000.csv"), "{html}");
    let at = |text: &str| html.find(text).unwrap();
    assert!(at("data-control=\"motion_test\"") < at("Measuring 3 / 1331"), "the progress follows the test row");
    dispatch(&mut app, "action", "Hardware:sweep_stop:", "").unwrap();
    assert!(stop.load(Ordering::SeqCst));
    drop(app.take_threads());
}

#[test]
fn an_unwritable_measurement_file_stops_before_the_sliders_are_reset() {
    let backend = FakeBackend::new();
    put(&backend, &["status", "documents"], json!("/proc/openrigcue-no-measurement-parent"));
    put(&backend, &["settings", "features", "motion_measurement_mode"], json!(true));
    let mut app = hardware(&backend);
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap_err();
    assert!(error.contains("/proc/openrigcue-no-measurement-parent/data_"), "{error}");
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(true));
    assert_eq!(backend.patches("controls"), [json!({"tests": {"motion_test": true}})], "no slider is reset");
    assert!(app.ui.devices.hardware.sweep.is_none());
    assert!(app.take_threads().is_empty(), "no sweep runs");
}

#[test]
fn the_motion_test_stops_unless_its_mode_is_numeric_zero() {
    for mode in [json!(""), serde_json::Value::Null, json!([]), json!({}), json!(true), json!(1)] {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        put(&backend, &["status", "controls", "tests", "motion_test"], mode.clone());
        // The stop does not look at the measurement flag.
        put(&backend, &["settings", "features", "motion_measurement_mode"], serde_json::Value::Null);
        dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
        assert_eq!(control(&backend, &["tests", "motion_test"]), json!(false), "{mode}");
        for key in ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"] {
            let value = control(&backend, &["tests", "motion_sliders", key]);
            assert!(value.is_i64() && value == json!(0), "{mode} {key}: {value}");
        }
        drop(app.take_threads());
    }
    // The service's current mode decides, not the screen's copy.
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    put(&backend, &["status", "controls", "tests", "motion_test"], json!(false));
    app.refresh_docs();
    put(&backend, &["status", "controls", "tests", "motion_test"], json!(true));
    dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(false));
    drop(app.take_threads());
}

#[test]
fn the_motion_test_toggle_needs_its_mode_and_stops_at_a_refused_write() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    backend.docs.lock().unwrap()["status"]["controls"]["tests"].as_object_mut().unwrap().shift_remove("motion_test");
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap_err();
    assert!(error.starts_with("KeyError"), "{error}");
    assert!(backend.patches("controls").is_empty());
    put(&backend, &["status", "controls", "tests", "motion_test"], json!(false));
    put(&backend, &["control_failure"], json!("RuntimeError: mode write failed"));
    backend.calls.lock().unwrap().clear();
    assert_eq!(
        dispatch(&mut app, "action", "Hardware:motion_test:", ""),
        Err("RuntimeError: mode write failed".into())
    );
    assert_eq!(backend.patches("controls"), [json!({"tests": {"motion_test": true}})], "no slider write follows");
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(false));
    drop(app.take_threads());
}

#[test]
fn the_motion_test_button_starts_and_stops_the_test_with_its_sliders() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["motion"]["link"]["status"] = json!("Connected");
    let mut app = hardware(&backend);
    choose_model(&mut app, &first_model(|m| m.name != "None").name).unwrap();
    app.refresh_docs();
    group(&mut app, "Motion");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
    let patches = backend.patches("controls");
    assert_eq!(patches[0], json!({"tests": {"motion_test": true}}));
    assert!(patches[1..].iter().all(|p| p["tests"]["motion_sliders"].is_object()) && patches.len() == 9);
    let html = main(&app);
    assert!(row_of(&html, "motion_test").contains(">Stop Test<"));
    assert!(!tag(&html, "c-hardware-heave").contains(" disabled"), "the sliders can be used");
    dispatch(&mut app, "set", "Hardware:heave", "50").unwrap();
    assert_eq!(control(&backend, &["tests", "motion_sliders", "heave"]), json!(50));
    dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(false));
    assert_eq!(control(&backend, &["tests", "motion_sliders", "heave"]), json!(0));
    let html = main(&app);
    assert!(row_of(&html, "motion_test").contains(">Start Test<"));
    assert!(tag(&html, "c-hardware-heave").contains(" disabled"));
    drop(app.take_threads());
}

#[test]
fn the_belt_test_button_writes_in_order_and_stops_at_a_refused_write() {
    for starting in [true, false] {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        put(&backend, &["status", "controls", "tests", "belt_test"], json!(!starting));
        put(&backend, &["status", "controls", "tests", "belt_test_value"], json!(17));
        backend.calls.lock().unwrap().clear();
        dispatch(&mut app, "action", "Hardware:belt_test:", "").unwrap();
        let expected = if starting {
            vec![
                json!({"switches": {"belt": true}}),
                json!({"tests": {"belt_test": true}}),
                json!({"tests": {"belt_test_value": 0}}),
            ]
        } else {
            vec![
                json!({"tests": {"belt_homing": 0}}),
                json!({"switches": {"belt": false}}),
                json!({"tests": {"belt_test": false}}),
                json!({"tests": {"belt_test_value": 0}}),
            ]
        };
        assert_eq!(backend.patches("controls"), expected);
        // A refused first write stops the toggle.
        put(&backend, &["status", "controls", "tests", "belt_test"], json!(!starting));
        put(&backend, &["control_failure"], json!("RuntimeError: belt toggle failed"));
        backend.calls.lock().unwrap().clear();
        assert_eq!(
            dispatch(&mut app, "action", "Hardware:belt_test:", ""),
            Err("RuntimeError: belt toggle failed".into())
        );
        assert_eq!(backend.patches("controls"), expected[..1]);
        drop(app.take_threads());
    }
}

// ---- Device rows and the values they store -------------------------------------------------------------------------

#[test]
fn device_rows_store_their_settings() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Hardware:auto_search_BELT_TENSIONER", "0").unwrap();
    dispatch(&mut app, "set", "Hardware:com_port_BELT_TENSIONER", "3").unwrap();
    dispatch(&mut app, "set", "Hardware:com_port_BELT_TENSIONER", "0").unwrap();
    dispatch(&mut app, "set", "Hardware:nbr_fans", "1").unwrap();
    dispatch(&mut app, "set", "Hardware:iflag_orientation", "1").unwrap();
    dispatch(&mut app, "set", "Hardware:wind_type", "1").unwrap();
    assert_eq!(
        backend.patches("settings"),
        [
            json!({"belt": {"auto_search": false}}),
            json!({"belt": {"serial_port": "COM3"}}),
            json!({"belt": {"serial_port": ""}}),
            json!({"wind": {"fan_count": "2"}}),
            json!({"iflag": {"rotation_degrees": 270}}),
            json!({"wind": {"fan_kind": "B"}}),
        ]
    );
    drop(app.take_threads());
}

#[test]
fn a_missing_settings_section_refuses_the_write() {
    for (section, control, value) in [
        ("belt", "auto_search_BELT_TENSIONER", "0"),
        ("belt", "com_port_BELT_TENSIONER", "7"),
        ("motion", "auto_search_MOTIONBOX", "1"),
        ("wind", "nbr_fans", "1"),
        ("leds", "led_model", "1"),
        ("leds", "nbr_leds", "120"),
        ("hud", "hud_model", "0"),
        ("iflag", "iflag_orientation", "1"),
        ("sim3d", "rumble_model", "1"),
        ("prosimu", "invert_tl", "1"),
        ("leds", "brightness", "31"),
        ("iflag", "iflag_dim_perc", "31"),
        ("belt", "belt_calibration_power_slider", "31"),
        ("belt", "belt_calibration_offset_slider", "31"),
    ] {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        put(&backend, &["settings", section], serde_json::Value::Null);
        let error = dispatch(&mut app, "set", &format!("Hardware:{control}"), value).unwrap_err();
        assert!(error.starts_with("TypeError:"), "{control}: {error}");
        assert_eq!(doc(&backend, &["settings", section]), serde_json::Value::Null, "{control}");
        drop(app.take_threads());
    }
}

#[test]
fn the_fan_count_of_two_offers_wind_curving_again_without_a_tune_write() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["wind"]["fan_count"] = json!("1");
    let mut app = hardware(&backend);
    put(&backend, &["tune", "speed_factor"], json!(9));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Hardware:nbr_fans", "1").unwrap();
    assert_eq!(native(&backend, "INTELLIBOX", "nbr_fans"), LegacyValue::Str("2".into()));
    assert!(called(&backend, Method::Patch, "profile").is_empty(), "two fans change no tune value");
    assert_eq!(tune(&backend, &["speed_factor"]), json!(9));
    navigate(&mut app, Screen::Wind, 3);
    assert!(!crate::ui::screens::main(&app).contains("Needs 2 fans (Hardware)"));
    drop(app.take_threads());
}

#[test]
fn an_unknown_choice_is_refused_without_a_write() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    backend.calls.lock().unwrap().clear();
    assert_eq!(wind_type(&mut app, "unknown"), Err("KeyError: 'unknown'".into()));
    assert!(dispatch(&mut app, "set", "Hardware:wind_type", "9").unwrap_err().starts_with("IndexError"));
    assert!(dispatch(&mut app, "set", "Hardware:nbr_fans", "x").unwrap_err().starts_with("ValueError"));
    assert!(dispatch(&mut app, "set", "Hardware:rumble_model", "3").unwrap_err().starts_with("IndexError"));
    assert!(backend.patches("settings").is_empty());
    drop(app.take_threads());
}

#[test]
fn the_rows_show_the_values_the_service_holds() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["wind"]["fan_kind"] = json!("B");
        docs["settings"]["belt"]["auto_search"] = json!(false);
        docs["settings"]["belt"]["calibration_factor"] = json!(43);
        docs["settings"]["belt"]["calibration_retraction"] = json!(57);
        docs["settings"]["iflag"]["brightness"] = json!(47);
        docs["settings"]["leds"]["brightness"] = json!(61);
    }
    // The main loop reads the service's documents again on each pass.
    app.main_loop_pass();
    let html = group(&mut app, "Intellibox");
    assert_eq!(options_of(&html, "c-hardware-wind_type").1.as_deref(), Some("Blower (discontinued)"));
    assert!(tag(&html, "c-hardware-brightness").contains("value=\"61\""));
    let html = group(&mut app, "Belt");
    assert!(!tag(&html, "c-hardware-auto_search_BELT_TENSIONER").contains(" checked"));
    assert_eq!(html.contains("c-hardware-com_port_BELT_TENSIONER"), cfg!(windows));
    assert!(tag(&html, "c-hardware-belt_calibration_power_slider").contains("value=\"43\""));
    assert!(tag(&html, "c-hardware-belt_calibration_offset_slider").contains("value=\"57\""));
    assert!(tag(&group(&mut app, "IFlag"), "c-hardware-iflag_dim_perc").contains("value=\"47\""));
    assert_eq!(doc(&backend, &["settings", "belt", "serial_port"]), json!(""), "showing writes no port");
    drop(app.take_threads());
}

#[test]
fn the_led_count_keeps_the_service_segments_and_their_other_fields() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    app.docs.settings["leds"]["segments"][0]["start"] = json!(1);
    app.docs.settings["leds"]["segments"][0]["end"] = json!(1);
    backend.docs.lock().unwrap()["settings"]["leds"]["segments"][0]["start"] = json!(7);
    backend.docs.lock().unwrap()["settings"]["leds"]["segments"][0]["end"] = json!(11);
    dispatch(&mut app, "set", "Hardware:nbr_leds", "150").unwrap();
    assert_eq!(native(&backend, "LED", "nbr_leds"), LegacyValue::Int(150));
    assert_eq!(doc(&backend, &["settings", "leds", "segments"])[0]["start"], json!(7));
    assert_eq!(doc(&backend, &["settings", "leds", "segments"])[0]["end"], json!(11));
    // A segment written on Tuning → LED keeps the segments' other fields.
    backend.docs.lock().unwrap()["settings"]["leds"]["segments"][0]["future_field"] = json!(17);
    backend.docs.lock().unwrap()["settings"]["leds"]["segments"][1]["kind"] = json!("flag");
    app.refresh_docs();
    dispatch(&mut app, "set", "LED:segment1", "3").unwrap();
    dispatch(&mut app, "set", "LED:segment1:high", "9").unwrap();
    let segments = doc(&backend, &["settings", "leds", "segments"]);
    assert_eq!((segments[0]["start"].clone(), segments[0]["end"].clone()), (json!(3), json!(9)));
    assert_eq!(segments[0]["future_field"], json!(17));
    assert_eq!(segments[1]["kind"], json!("flag"));
    drop(app.take_threads());
}

#[test]
fn the_switch_rows_store_integers() {
    let backend = FakeBackend::new();
    let model = first_model(|m| m.allow_change_tl_dir);
    let mut app = hardware(&backend);
    choose_model(&mut app, &model.name).unwrap();
    dispatch(&mut app, "set", "Hardware:invert_tl", "1").unwrap();
    assert_eq!(doc(&backend, &["settings", "prosimu", "invert_traction_loss"]), json!(1));
    dispatch(&mut app, "set", "Hardware:auto_smoothing_low_speed", "1").unwrap();
    assert_eq!(doc(&backend, &["settings", "motion", "auto_smoothing_low_speed"]), json!(1));
    for (control, section, key) in [
        ("brightness", "leds", "brightness"),
        ("iflag_dim_perc", "iflag", "brightness"),
        ("belt_calibration_power_slider", "belt", "calibration_factor"),
        ("belt_calibration_offset_slider", "belt", "calibration_retraction"),
    ] {
        dispatch(&mut app, "set", &format!("Hardware:{control}"), "23").unwrap();
        let stored = doc(&backend, &["settings", section, key]);
        assert!(stored.is_i64() && stored == json!(23), "{control}: {stored}");
        backend.calls.lock().unwrap().clear();
        // Only a whole number as typed is stored: a fraction is refused, not rounded.
        for bad in ["NaN", "inf", "x", "", "1.5", "23.0"] {
            assert!(dispatch(&mut app, "set", &format!("Hardware:{control}"), bad).is_err(), "{control} {bad}");
        }
        assert!(backend.patches("settings").is_empty(), "{control}");
    }
    drop(app.take_threads());
}

// ---- Start-up --------------------------------------------------------------------------------------------------------

#[test]
fn start_stops_at_a_failed_effects_load_and_stores_the_layout_first() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    assert!(choose_layout(&mut app, "Missing layout").unwrap_err().starts_with("KeyError"));
    assert_eq!(native(&backend, "SHAKEBOX", "profile"), LegacyValue::Str("Missing layout".into()));
    backend.calls.lock().unwrap().clear();
    app.set_motion_setting_native_checked("model", &LegacyValue::Str("None".into())).unwrap();
    backend.calls.lock().unwrap().clear();
    assert!(start(&mut app).unwrap_err().starts_with("KeyError"));
    assert!(!called(&backend, Method::Put, "shakers/soundcard").is_empty(), "the sound device is opened first");
    assert!(backend.patches("settings").is_empty() && called(&backend, Method::Patch, "profile").is_empty());
    drop(app.take_threads());
}

#[test]
fn the_layout_and_sound_device_rows_show_the_stored_ones() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let names: Vec<String> = layouts(&app).into_iter().map(|c| c.label).collect();
    let other = names.iter().find(|n| **n != setting_text(&app, "SHAKEBOX", "profile")).unwrap().clone();
    put(&backend, &["settings", "shakers", "speaker_layout"], json!(other));
    put(&backend, &["settings", "shakers", "sound_card"], json!("USB Sound Device"));
    app.refresh_docs();
    let html = group(&mut app, "Shakers");
    assert_eq!(options_of(&html, "c-hardware-shakers_profile").1, Some(tr(&app, &other)));
    assert_eq!(options_of(&html, "c-hardware-soundcard").1.as_deref(), Some("USB Sound Device"));
    drop(app.take_threads());
}

#[test]
fn start_goes_on_after_the_model_rules_fail() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let model = first_model(|m| m.name != "None" && !m.enable_intellicomp);
    put(&backend, &["settings", "motion", "model"], json!(model.name));
    put(&backend, &["settings", "compensation"], serde_json::Value::Null);
    put(&backend, &["settings", "wind", "fan_count"], json!("1"));
    put(&backend, &["tune", "wind_curving"], json!(1));
    assert_eq!(start(&mut app), Ok(()));
    assert_eq!(
        native(&backend, "MOTIONBOX", "model"),
        LegacyValue::Str("None".into()),
        "the failing model is replaced"
    );
    assert_eq!(tune(&backend, &["wind_curving"]), json!(0), "the one-fan rule after the model still runs");
    drop(app.take_threads());
}

// ---- The motion model and its ports ----------------------------------------------------------------------------------

#[test]
fn an_unknown_manufacturer_or_model_is_refused() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    backend.calls.lock().unwrap().clear();
    assert_eq!(choose_manufacturer(&mut app, "Unknown"), Err("KeyError: 'Unknown'".into()));
    assert!(backend.patches("settings").is_empty());
    assert_eq!(choose_model(&mut app, "Unknown"), Err("KeyError: 'Unknown'".into()));
    assert_eq!(native(&backend, "MOTIONBOX", "model"), LegacyValue::Str("Unknown".into()), "stored before the rules");
    put(&backend, &["settings", "motion"], serde_json::Value::Null);
    let error = choose_model(&mut app, &first_model(|m| m.name != "None").name).unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert_eq!(doc(&backend, &["settings", "motion"]), serde_json::Value::Null);
    drop(app.take_threads());
}

#[test]
fn a_missing_axis_assignment_stops_the_model_rules_before_the_ports() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    put(&backend, &["tune", "min_traction_loss_front_activation"], json!(30));
    backend.docs.lock().unwrap()["settings"]["motion"].as_object_mut().unwrap().shift_remove("motor_axes");
    backend.calls.lock().unwrap().clear();
    let model = first_model(|m| m.manufacturer == "VNM SIMULATION");
    let error = choose_model(&mut app, &model.name).unwrap_err();
    assert!(error.starts_with("KeyError"), "{error}");
    assert_eq!(native(&backend, "MOTIONBOX", "model"), LegacyValue::Str(model.name.clone()));
    assert!(called(&backend, Method::Get, "hardware").is_empty(), "the ports are not listed again");
    assert_eq!(tune(&backend, &["min_traction_loss_front_activation"]), json!(30));
    drop(app.take_threads());
}

#[test]
fn a_model_without_intellicomp_needs_the_compensation_section() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    put(&backend, &["settings", "compensation"], serde_json::Value::Null);
    let model = first_model(|m| m.name != "None" && !m.enable_intellicomp);
    assert_eq!(
        choose_model(&mut app, &model.name),
        Err("TypeError: 'NoneType' object does not support item assignment".into())
    );
    assert_eq!(doc(&backend, &["settings", "compensation"]), serde_json::Value::Null);
    drop(app.take_threads());
}

#[test]
fn the_ports_follow_the_model_the_service_holds_after_the_listing() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        // The service's model changes while the ports are listed.
        docs["motion_model_after_enumeration"] = json!("T1000-5M PRS200 (Upgrade)");
        docs["settings"]["prosimu"]["prs200_serial_port_1"] = json!("COM9");
        docs["settings"]["prosimu"]["prs200_serial_port_2"] = json!("COM10");
        docs["hardware"]["serial_ports"] = json!([
            {"device": "COM8", "description": "other", "hwid": ""},
            {"device": "COM9", "description": "first", "hwid": "0403:6001"},
            {"device": "COM10", "description": "second", "hwid": "0403:6001"}
        ]);
    }
    choose_model(&mut app, &first_model(|m| m.manufacturer == "VNM SIMULATION").name).unwrap();
    app.refresh_docs();
    let html = group(&mut app, "Motion");
    let (first, chosen) = options_of(&html, "c-hardware-motion_port");
    assert_eq!((first, chosen.as_deref()), (vec!["".to_owned(), "COM9".into(), "COM10".into()], Some("COM9")));
    let (second, chosen) = options_of(&html, "c-hardware-motion_port2");
    assert_eq!((second, chosen.as_deref()), (vec!["".to_owned(), "COM9".into(), "COM10".into()], Some("COM10")));
    drop(app.take_threads());
}

fn manual_port_options(backend: &std::sync::Arc<FakeBackend>) -> Vec<String> {
    let mut app = hardware(backend);
    choose_model(&mut app, &first_model(|m| m.manual_com_port).name).unwrap();
    let html = group(&mut app, "Motion");
    drop(app.take_threads());
    options_of(&html, "c-hardware-motion_port").0
}

#[test]
fn the_manual_port_leaves_out_the_ports_held_elsewhere() {
    let ports = json!([
        {"device": "COM7", "description": "held", "hwid": "USB VID:PID=0403:6001"},
        {"device": "COM8", "description": "free", "hwid": "USB VID:PID=0403:6001"}
    ]);
    // The traction-loss board's port.
    let backend = FakeBackend::new();
    put(&backend, &["status", "motion", "assignment"], json!({"tl_port": "COM7"}));
    put(&backend, &["hardware", "serial_ports"], ports.clone());
    assert_eq!(manual_port_options(&backend), ["", "COM8"]);
    // Each accessory's port.
    for key in ["compensation_port", "belt_port", "intellibox_port", "iflag_port", "sim3d_port"] {
        let backend = FakeBackend::new();
        put(&backend, &["status", "accessories", key], json!("COM7"));
        put(&backend, &["hardware", "serial_ports"], ports.clone());
        assert_eq!(manual_port_options(&backend), ["", "COM8"], "{key}");
    }
    // A held port given as a number is not the port named by that text.
    for path in [vec!["status", "motion", "assignment", "tl_port"], vec!["status", "accessories", "compensation_port"]]
    {
        let backend = FakeBackend::new();
        put(&backend, &path, json!(17));
        put(&backend, &["hardware", "serial_ports"], json!([{"device": "17", "hwid": "USB VID:PID=0403:6001"}]));
        assert_eq!(manual_port_options(&backend), ["", "17"], "{path:?}");
    }
}

#[test]
fn a_manual_port_without_a_stored_one_shows_no_choice() {
    let backend = FakeBackend::new();
    let model = first_model(|m| m.manual_com_port);
    let (section, key) = port_setting(&model.manufacturer);
    let path = match (section, key) {
        ("VNM SIMULATION", _) => ["vnm", "serial_port"],
        ("PT-ACTUATOR", _) => ["pt_actuator", "canbus_serial_port"],
        _ => ["prosimu", "prs200_serial_port_1"],
    };
    put(&backend, &["settings", path[0], path[1]], serde_json::Value::Null);
    put(&backend, &["hardware", "serial_ports"], json!([{"device": "COM8", "hwid": "USB VID:PID=0403:6001"}]));
    let mut app = hardware(&backend);
    choose_model(&mut app, &model.name).unwrap();
    let html = group(&mut app, "Motion");
    assert_eq!(options_of(&html, "c-hardware-motion_port").1, None, "the empty entry is not chosen");
    assert!(row_of(&html, "motion_port").contains("<option value=\"\" selected disabled>—</option>"));
    drop(app.take_threads());
}

#[test]
fn the_prosimu_ports_are_stored_under_their_keys_and_need_their_section() {
    let backend = FakeBackend::new();
    put(
        &backend,
        &["hardware", "serial_ports"],
        json!([{"device": "COM9", "hwid": "0403:6001"}, {"device": "COM10", "hwid": "0403:6001"}]),
    );
    let mut app = hardware(&backend);
    choose_model(&mut app, "T1000-5M PRS200 (Upgrade)").unwrap();
    dispatch(&mut app, "set", "Hardware:motion_port", "1").unwrap();
    dispatch(&mut app, "set", "Hardware:motion_port2", "2").unwrap();
    assert_eq!(native(&backend, "PROSIMU", "prs200_com_port1"), LegacyValue::Str("COM9".into()));
    assert_eq!(native(&backend, "PROSIMU", "prs200_com_port2"), LegacyValue::Str("COM10".into()));
    dispatch(&mut app, "set", "Hardware:motion_port2", "0").unwrap();
    assert_eq!(native(&backend, "PROSIMU", "prs200_com_port2"), LegacyValue::Str(String::new()));
    put(&backend, &["settings", "prosimu"], serde_json::Value::Null);
    let error = dispatch(&mut app, "set", "Hardware:motion_port", "1").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert_eq!(doc(&backend, &["settings", "prosimu"]), serde_json::Value::Null);
    drop(app.take_threads());
}

// ---- The per-pass rules and the test buttons ---------------------------------------------------------------------------

#[test]
fn the_speaker_rows_follow_the_sound_device_and_the_layout() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["layouts"] = json!([
            {"name": "SRS UShake6", "builtin": true,
             "speakers": {"FL": true, "FR": true, "CE": false, "SUB": true, "RL": true, "RR": true}},
            {"name": "My Layout", "builtin": false,
             "speakers": {"FL": true, "FR": true, "CE": true, "SUB": true, "RL": true, "RR": true}}
        ]);
        docs["settings"]["shakers"]["speaker_layout"] = json!("SRS UShake6");
    }
    let mut app = hardware(&backend);
    let html = group(&mut app, "Shakers");
    // No sound device: every speaker off, the layout buttons off; a built-in layout cannot be deleted.
    for key in ["fl", "ce"] {
        assert!(tag(&html, &format!("c-hardware-{key}")).contains(" disabled"), "{key}");
        assert!(
            !button_enabled(&html, &format!("Hardware:{key}:0"))
                && !button_enabled(&html, &format!("Hardware:{key}:1"))
        );
    }
    assert!(
        !button_enabled(&html, "Hardware:shakers_profile:0") && !button_enabled(&html, "Hardware:shakers_profile:1")
    );
    // A sound device and a custom layout: every speaker it uses, Delete and the matrix.
    app.set_setting_native_checked("SHAKEBOX", "soundcard", &LegacyValue::Str("USB Sound Device".into())).unwrap();
    let index = option_index(&main(&app), "c-hardware-shakers_profile", "My Layout");
    dispatch(&mut app, "set", "Hardware:shakers_profile", &index).unwrap();
    let html = main(&app);
    assert!(button_enabled(&html, "Hardware:shakers_profile:0") && button_enabled(&html, "Hardware:shakers_profile:1"));
    assert!(!tag(&html, "c-hardware-ce").contains(" disabled"));
    assert!(button_enabled(&html, "Hardware:ce:0") && button_enabled(&html, "Hardware:ce:1"));
    // The built-in layout again: its unused speaker is off and no matrix is offered.
    let index = option_index(&html, "c-hardware-shakers_profile", "SRS UShake6");
    dispatch(&mut app, "set", "Hardware:shakers_profile", &index).unwrap();
    let html = main(&app);
    assert!(tag(&html, "c-hardware-ce").contains(" disabled") && !tag(&html, "c-hardware-fl").contains(" disabled"));
    assert!(button_enabled(&html, "Hardware:fl:0") && !button_enabled(&html, "Hardware:fl:1"));
    assert!(!button_enabled(&html, "Hardware:shakers_profile:1"));
    drop(app.take_threads());
}

#[test]
fn a_loaded_profile_shows_its_shift_light_and_speaker_volumes() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    put(&backend, &["tune", "gear_shift_dim_perc"], json!(37));
    put(&backend, &["tune", "shaker_volume", "rr"], json!(7));
    app.main_loop_pass();
    assert_eq!(shown_value(&app, "Intellibox", "shift_light_dim_perc"), 37.0);
    assert_eq!(shown_value(&app, "Shakers", "rr"), 7.0);
    assert_eq!(tune(&backend, &["shaker_volume", "rr"]), json!(7), "showing writes nothing");
    drop(app.take_threads());
}

#[test]
fn the_speaker_tests_play_only_with_a_sound_device_and_the_matrix_shows_the_levels() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    group(&mut app, "Shakers");
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Hardware:sub", "10").unwrap();
    assert_eq!(called(&backend, Method::Patch, "profile"), [Some(json!({"shaker_volume": {"sub": 10}}))]);
    dispatch(&mut app, "action", "Hardware:sub:0", "").unwrap();
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert!(called(&backend, Method::Post, "shakers/speakers/SUB/test").is_empty(), "no sound device");
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    app.set_setting_native_checked("SHAKEBOX", "soundcard", &LegacyValue::Str("USB Sound Device".into())).unwrap();
    dispatch(&mut app, "action", "Hardware:sub:0", "").unwrap();
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert_eq!(called(&backend, Method::Post, "shakers/speakers/SUB/test").len(), 1);
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    backend.docs.lock().unwrap()["loaded_shakers"] = json!({"SUB": {"enable": true, "sfx": {"engine": 7, "gear": 5}}});
    dispatch(&mut app, "action", "Hardware:sub:1", "").unwrap();
    let Some(Device::Matrix { speaker, levels }) = dh::device(&app).cloned() else { panic!("matrix dialog") };
    assert_eq!((speaker.as_str(), levels.item("engine").cloned()), ("SUB", Ok(LegacyValue::Int(7))));
    drop(app.take_threads());
}

#[test]
fn the_controller_buttons_need_a_connected_platform() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    choose_model(&mut app, &first_model(|m| m.manufacturer == "DOF REALITY").name).unwrap();
    let html = group(&mut app, "Motion");
    for index in 0..6 {
        assert!(!html.contains(&format!("data-arg=\"Hardware:motion_model:{index}\"")), "{index}");
    }
    backend.docs.lock().unwrap()["status"]["motion"]["link"]["status"] = json!("Connected");
    let html = main(&app);
    assert!(html.contains("data-arg=\"Hardware:motion_model:5\""), "DOF Reality's settings");
    assert!(
        !html.contains("data-arg=\"Hardware:motion_model:2\"")
            && !html.contains("data-arg=\"Hardware:motion_model:4\"")
    );
    drop(app.take_threads());
}

#[test]
fn the_hapticr_tests_need_hapticr_enabled() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["app"]["enable_hapticr"] = json!(true);
        docs["status"]["accessories"]["hapticr"] = json!("Connected");
    }
    let mut app = hardware(&backend);
    let html = group(&mut app, "HapticR");
    assert!(
        button_enabled(&html, "Hardware:haptic_test_front:") && button_enabled(&html, "Hardware:haptic_test_rear:0")
    );
    for (flag, on) in [(json!(false), false), (json!("y"), true), (json!("no"), false)] {
        backend.docs.lock().unwrap()["settings"]["app"]["enable_hapticr"] = flag.clone();
        let rules = Rules::read(&app);
        assert_eq!(rules.haptic_ready(), on, "{flag}");
        let html = main_of(&app, 8);
        assert_eq!(button_enabled(&html, "Hardware:haptic_test_front:"), on, "{flag}");
        assert_eq!(button_enabled(&html, "Hardware:haptic_test_front:0"), on, "{flag}");
    }
    drop(app.take_threads());
}

#[test]
fn each_connected_accessory_enables_its_test() {
    for (device, title, arg) in [
        ("iflag", "IFlag", "Hardware:iflag_test:"),
        ("sim3d", "SIM3D", "Hardware:rumble_port1:0"),
        ("hapticr", "HapticR", "Hardware:haptic_test_front:"),
        ("belt", "Belt", "Hardware:belt_test:"),
    ] {
        let backend = FakeBackend::new();
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["settings"]["app"]["enable_hapticr"] = json!(true);
            docs["settings"]["sim3d"]["model"] = json!("3 Ports");
        }
        let mut app = hardware(&backend);
        assert!(!button_enabled(&group(&mut app, title), arg), "{device} not connected");
        backend.docs.lock().unwrap()["status"]["accessories"][device] = json!("Connected");
        assert!(button_enabled(&main(&app), arg), "{device} connected");
        // A running game keeps the test off.
        backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("acc");
        assert!(!button_enabled(&main(&app), arg), "{device} while a game runs");
        drop(app.take_threads());
    }
}

#[test]
fn an_iflag_or_hapticr_test_running_elsewhere_keeps_its_buttons_off() {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["app"]["enable_hapticr"] = json!(true);
        docs["status"]["accessories"]["iflag"] = json!("Connected");
        docs["status"]["accessories"]["hapticr"] = json!("Connected");
    }
    let mut app = hardware(&backend);
    for (title, arg, key) in
        [("IFlag", "Hardware:iflag_test:", "iflag_test"), ("HapticR", "Hardware:haptic_test_front:", "haptic_test")]
    {
        assert!(button_enabled(&group(&mut app, title), arg), "{title} idle");
        // A test another client or a shortcut started; any value but the number 0 counts as running.
        for running in [json!(true), json!(2), json!("0"), serde_json::Value::Null] {
            backend.docs.lock().unwrap()["status"]["controls"]["tests"][key] = running.clone();
            assert!(!button_enabled(&main(&app), arg), "{title} while {key} = {running}");
        }
        for idle in [json!(0), json!(false), json!(0.0)] {
            backend.docs.lock().unwrap()["status"]["controls"]["tests"][key] = idle.clone();
            assert!(button_enabled(&main(&app), arg), "{title} with {key} = {idle}");
        }
    }
    drop(app.take_threads());
}

#[test]
fn a_model_change_reports_a_device_list_that_cannot_be_read() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let model = first_model(|m| m.name != "None");
    backend.docs.lock().unwrap()["hardware_failure"] = json!("RuntimeError: enumeration failed");
    let error = choose_model(&mut app, &model.name).unwrap_err();
    assert!(error.contains("enumeration failed"), "{error}");
    assert_eq!(native(&backend, "MOTIONBOX", "model"), LegacyValue::Str(model.name.clone()), "the model is stored");
    drop(app.take_threads());
}

#[test]
fn the_none_sound_device_is_shown_in_the_selected_language() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let (shown, _) = options_of(&group(&mut app, "Shakers"), "c-hardware-soundcard");
    assert!(shown.contains(&crate::ui::html::tr(&app, "None")), "{shown:?}");
    assert!(shown.contains(&"USB Sound Device".to_owned()), "a device name is shown as it is");
    drop(app.take_threads());
}

/// The fake service with one route held at a gate until the test releases it.
struct Gated {
    inner: std::sync::Arc<FakeBackend>,
    method: Method,
    path: &'static str,
    entered: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Gated {
    /// Installs the gate; returns the receiver told when the call arrives and the sender that lets it go on.
    fn install(
        app: &mut MyApp,
        inner: &std::sync::Arc<FakeBackend>,
        method: Method,
        path: &'static str,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered_send, entered) = std::sync::mpsc::channel();
        let (release, release_receive) = std::sync::mpsc::channel();
        app.backend = std::sync::Arc::new(Gated {
            inner: inner.clone(),
            method,
            path,
            entered: std::sync::Mutex::new(Some(entered_send)),
            release: std::sync::Mutex::new(Some(release_receive)),
        });
        (entered, release)
    }
}

impl Backend for Gated {
    fn call(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, Failure> {
        if method == self.method && path == self.path {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                entered.send(()).unwrap();
                self.release.lock().unwrap().take().unwrap().recv().unwrap();
            }
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        self.inner.native_setting(section, key)
    }
    fn hardware_parameter(&self, name: &str) -> Result<LegacyValue, Failure> {
        self.inner.hardware_parameter(name)
    }
    fn ui_actions(&self) -> Vec<String> {
        self.inner.ui_actions()
    }
    fn clear_ui_actions(&self) {
        self.inner.clear_ui_actions()
    }
    fn upload(&self, path: &str, bytes: Vec<u8>) -> Result<serde_json::Value, Failure> {
        self.inner.upload(path, bytes)
    }
    fn fetch(&self, path: &str) -> Result<(Vec<u8>, String), Failure> {
        self.inner.fetch(path)
    }
    fn translate(&self, text: &str) -> String {
        self.inner.translate(text)
    }
    fn excluded_strings(&self) -> Vec<String> {
        self.inner.excluded_strings()
    }
}

/// While the gated call waits, the application is free; afterwards the work has been taken.
fn runs_without_holding_the_application(
    app: MyApp,
    (entered, release): (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>),
    work: impl FnOnce(std::sync::Arc<std::sync::Mutex<MyApp>>) -> Result<(), String> + Send + 'static,
) -> MyApp {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(app));
    let worker = {
        let shared = shared.clone();
        std::thread::spawn(move || work(shared))
    };
    entered.recv().unwrap();
    assert!(shared.try_lock().is_ok(), "the application is not held while the service answers");
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    std::sync::Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap()
}

#[test]
fn the_sound_device_refresh_reads_the_list_without_holding_the_application() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["shakers"]["refresh_soundcard"] = json!(true);
        docs["hardware"]["sound_cards"] = json!(["None", "Fresh Device"]);
    }
    let gate = Gated::install(&mut app, &backend, Method::Get, "hardware");
    let mut app = runs_without_holding_the_application(app, gate, |shared| main_loop_rules_shared(&shared));
    let (shown, _) = options_of(&group(&mut app, "Shakers"), "c-hardware-soundcard");
    assert!(shown.contains(&"Fresh Device".to_owned()), "{shown:?}");
    assert_eq!(doc(&backend, &["status", "shakers", "refresh_soundcard"]), json!(false));
    drop(app.take_threads());
}

#[test]
fn a_finished_device_test_restores_its_output_without_holding_the_application() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["accessories"]["iflag"] = json!("Connected");
    let mut app = hardware(&backend);
    dispatch(&mut app, "action", "Hardware:iflag_test:", "").unwrap();
    dispatch(&mut app, "dialog-button", "start", "").unwrap();
    assert_eq!(control(&backend, &["tests", "iflag_test"]), json!(true));
    let mut jobs = app.take_threads();
    assert_eq!(jobs.len(), 1, "one timer ends the test");
    let job = jobs.pop().unwrap();
    let gate = Gated::install(&mut app, &backend, Method::Patch, "controls");
    let mut app = runs_without_holding_the_application(app, gate, move |shared| {
        // The job holds only a weak handle, as on the running application.
        (job.run)(std::sync::Arc::downgrade(&shared));
        Ok(())
    });
    assert_eq!(control(&backend, &["tests", "iflag_test"]), json!(false));
    assert!(app.ui.devices.hardware.test.is_none());
    drop(app.take_threads());
}

#[test]
fn the_sim3d_tests_follow_the_model_the_service_holds() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["accessories"]["sim3d"] = json!("Connected");
    let mut app = hardware(&backend);
    let html = group(&mut app, "SIM3D");
    assert!(!html.contains("Hardware:rumble_port1\""), "no port without a model");
    backend.docs.lock().unwrap()["settings"]["sim3d"]["model"] = json!("3 Ports");
    assert!(button_enabled(&main(&app), "Hardware:rumble_port1:0"));
    drop(app.take_threads());
}

#[test]
fn the_belt_test_slider_needs_the_calibrated_running_test() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["switches"]["belt"] = json!(true);
        docs["status"]["controls"]["tests"]["belt_homing"] = json!(1);
        docs["status"]["controls"]["tests"]["belt_test"] = json!(true);
    }
    rules(&mut app).unwrap();
    let html = group(&mut app, "Belt");
    assert!(!tag(&html, "c-hardware-belt_test_slider").contains(" disabled"));
    assert_eq!(control(&backend, &["tests", "belt_test"]), json!(true), "the belt switched on keeps the test");
    backend.docs.lock().unwrap()["status"]["controls"]["tests"]["belt_homing"] = json!(3);
    assert!(tag(&main(&app), "c-hardware-belt_test_slider").contains(" disabled"));
    drop(app.take_threads());
}

#[test]
fn the_pass_rules_clear_the_service_modes() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["belt"] = json!(false);
    let mut app = hardware(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["tests"]["motion_test"] = json!(true);
        docs["status"]["controls"]["tests"]["belt_test"] = json!(true);
        docs["status"]["controls"]["tests"]["belt_homing"] = json!(3);
    }
    rules(&mut app).unwrap();
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(false));
    assert_eq!(control(&backend, &["tests", "belt_test"]), json!(false));
    assert_eq!(control(&backend, &["tests", "belt_homing"]), json!(0));
    drop(app.take_threads());
}

#[test]
fn reverse_traction_loss_shows_the_service_value_for_its_models() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    assert!(!rows_shown(&group(&mut app, "Motion")).contains(&"invert_tl".to_owned()), "not for the None model");
    choose_model(&mut app, &first_model(|m| m.allow_change_tl_dir).name).unwrap();
    assert!(!tag(&main(&app), "c-hardware-invert_tl").contains(" checked"));
    backend.docs.lock().unwrap()["settings"]["prosimu"]["invert_traction_loss"] = json!(true);
    assert!(tag(&main(&app), "c-hardware-invert_tl").contains(" checked"));
    drop(app.take_threads());
}

// ---- The sound device list ----------------------------------------------------------------------------------------------

#[test]
fn a_sound_device_refresh_reads_the_service_request() {
    for (cached, current) in [(false, true), (true, false)] {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        app.docs.status["shakers"]["refresh_soundcard"] = json!(cached);
        put(&backend, &["status", "shakers", "refresh_soundcard"], json!(current));
        put(&backend, &["hardware", "sound_cards"], json!(["None", "New USB Card"]));
        put(&backend, &["settings", "shakers", "sound_card"], json!("New USB Card"));
        app.refresh_docs();
        app.docs.status["shakers"]["refresh_soundcard"] = json!(cached);
        backend.calls.lock().unwrap().clear();
        rules(&mut app).unwrap();
        let html = group(&mut app, "Shakers");
        let (cards, chosen) = options_of(&html, "c-hardware-soundcard");
        assert_eq!(cards.contains(&"New USB Card".to_owned()), current);
        assert_eq!(chosen.is_some(), current, "the stored device is chosen after the refresh");
        assert_eq!(called(&backend, Method::Delete, "shakers/refresh-soundcard").len(), usize::from(current));
        assert_eq!(called(&backend, Method::Put, "shakers/soundcard").len(), usize::from(current), "opened again");
        drop(app.take_threads());
    }
}

#[test]
fn the_sound_devices_keep_their_identity_when_their_names_change() {
    let backend = FakeBackend::new();
    put(
        &backend,
        &["hardware", "sound_devices"],
        json!([
            {"id": "None", "name": "None"},
            {"id": "pulse:usb.spdif", "name": "USB Audio S/PDIF Output"},
            {"id": "pulse:usb.speakers", "name": "USB Audio Speakers"},
            {"id": "pulse:x4", "name": "Sound Blaster X4 Analog Stereo"},
            {"id": "pulse:nvidia", "name": "GB202 High Definition Audio Controller Digital Stereo (HDMI)"}
        ]),
    );
    put(&backend, &["settings", "shakers", "sound_card"], json!("pulse:usb.speakers"));
    let mut app = hardware(&backend);
    let html = group(&mut app, "Shakers");
    let (names, chosen) = options_of(&html, "c-hardware-soundcard");
    assert_eq!(names.len(), 5);
    assert_eq!(chosen.as_deref(), Some("USB Audio Speakers"));
    assert!(html.contains("<strong>Sound Device</strong>"));
    let index = option_index(&html, "c-hardware-soundcard", "USB Audio S/PDIF Output");
    dispatch(&mut app, "set", "Hardware:soundcard", &index).unwrap();
    assert_eq!(native(&backend, "SHAKEBOX", "soundcard"), LegacyValue::Str("pulse:usb.spdif".into()));
    put(
        &backend,
        &["hardware", "sound_devices"],
        json!([
            {"id": "None", "name": "None"},
            {"id": "pulse:usb.spdif", "name": "Renamed Digital Output"},
            {"id": "pulse:usb.speakers", "name": "USB Audio Speakers"}
        ]),
    );
    put(&backend, &["status", "shakers", "refresh_soundcard"], json!(true));
    rules(&mut app).unwrap();
    assert_eq!(options_of(&main(&app), "c-hardware-soundcard").1.as_deref(), Some("Renamed Digital Output"));
    assert_eq!(native(&backend, "SHAKEBOX", "soundcard"), LegacyValue::Str("pulse:usb.spdif".into()));
    drop(app.take_threads());
}

#[test]
fn a_sound_device_that_does_not_open_is_reported_and_stays_stored() {
    let backend = FakeBackend::new();
    put(
        &backend,
        &["hardware", "sound_devices"],
        json!([{"id": "None", "name": "None"}, {"id": "pulse:usb.speakers", "name": "USB Audio Speakers"}]),
    );
    let mut app = hardware(&backend);
    app.backend = std::sync::Arc::new(Failing {
        inner: backend.clone(),
        path: "shakers/soundcard",
        detail: "selected endpoint disconnected".into(),
    });
    let html = group(&mut app, "Shakers");
    let index = option_index(&html, "c-hardware-soundcard", "USB Audio Speakers");
    dispatch(&mut app, "set", "Hardware:soundcard", &index).unwrap();
    assert_eq!(native(&backend, "SHAKEBOX", "soundcard"), LegacyValue::Str("pulse:usb.speakers".into()));
    let shown = alert_text(&app).unwrap();
    assert!(
        shown.contains("Could not open the selected sound device") && shown.contains("selected endpoint disconnected")
    );
    app.backend = backend.clone();
    drop(app.take_threads());
}

#[test]
fn the_sound_device_stored_is_the_entry_not_its_translation() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["translations"] = json!({"None": "Aucune"});
    put(&backend, &["settings", "shakers", "sound_card"], json!("USB Sound Device"));
    let mut app = hardware(&backend);
    let html = group(&mut app, "Shakers");
    assert_eq!(options_of(&html, "c-hardware-soundcard").1.as_deref(), Some("USB Sound Device"));
    dispatch(&mut app, "set", "Hardware:soundcard", "0").unwrap();
    assert_eq!(doc(&backend, &["settings", "shakers", "sound_card"]), json!("None"));
    dispatch(&mut app, "set", "Hardware:soundcard", "1").unwrap();
    assert_eq!(doc(&backend, &["settings", "shakers", "sound_card"]), json!("USB Sound Device"));
    drop(app.take_threads());
}

// ---- Number boxes and the measurement sweep ---------------------------------------------------------------

/// A folder of this test run for the files a test writes (under the build's target folder).
fn test_folder(name: &str) -> std::path::PathBuf {
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/openrigcue-ui-test-files")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    folder
}

/// The LED count box keeps a typed count within 1…150 before it is stored, a fraction dropped; text that is no
/// number stores the count the box shows; a NaN stores nothing.
#[test]
fn the_led_count_box_keeps_a_typed_count_within_its_limits() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    for (typed, stored) in [("500", 150), ("0", 1), ("-4", 1), ("37.9", 37), ("1e9", 150), ("inf", 150), ("0.4", 1)] {
        dispatch(&mut app, "set", "Hardware:nbr_leds", typed).unwrap();
        assert_eq!(native(&backend, "LED", "nbr_leds"), LegacyValue::Int(stored), "{typed:?}");
    }
    dispatch(&mut app, "set", "Hardware:nbr_leds", "42").unwrap();
    for unreadable in ["", "-", "abc"] {
        backend.calls.lock().unwrap().clear();
        dispatch(&mut app, "set", "Hardware:nbr_leds", unreadable).unwrap();
        assert_eq!(native(&backend, "LED", "nbr_leds"), LegacyValue::Int(42), "{unreadable:?}");
        assert!(!backend.patches("settings").is_empty(), "{unreadable:?}: the shown count is stored again");
    }
    backend.calls.lock().unwrap().clear();
    assert!(dispatch(&mut app, "set", "Hardware:nbr_leds", "nan").is_err());
    assert!(backend.patches("settings").is_empty());
    assert!(tag(&group(&mut app, "Intellibox"), "c-hardware-nbr_leds").contains("min=\"1\" max=\"150\""));
    drop(app.take_threads());
}

/// Every number box of a page description is taken through its limits before its screen sees the value: the
/// LED segment count, the Motion thresholds and spike filters, each effect's Max Telemetry box and the Motion
/// Compensation and LED count boxes. The box shows the stored value (a fraction dropped where the row reads whole
/// numbers).
#[test]
fn every_number_box_of_a_page_is_taken_through_its_limits() {
    use crate::ui::screens::number_box;
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["tune"]["thanos_spike_filter_vertical"] = json!(300);
    let mut app = hardware(&backend);
    app.refresh_docs();
    let limits = |app: &MyApp, screen: Screen, control: &str, part: &str| {
        number_box(app, screen, control, part).map(|(b, shown)| (b.min, b.max, b.step, shown))
    };
    assert_eq!(
        limits(&app, Screen::Led, "nbr_segments", ""),
        Some((1.0, 4.0, 1.0, num(app.setting(&["leds", "nbr_segments"]))))
    );
    assert_eq!(limits(&app, Screen::Hardware, "nbr_leds", "").map(|l| (l.0, l.1)), Some((1.0, 150.0)));
    for control in ["min_traction_loss_activation", "min_traction_loss_front_activation"] {
        assert_eq!(
            limits(&app, Screen::Motion, control, "").map(|l| (l.0, l.1, l.2)),
            Some((0.0, 1800.0, 1.0)),
            "{control}"
        );
    }
    assert_eq!(
        limits(&app, Screen::Motion, "thanos_spike_filter_vertical", ""),
        Some((255.0, 64530.0, 30.0, "300".to_owned()))
    );
    let page = crate::ui::screens::page(&app, Screen::Motion).unwrap();
    let effect = page.controls().find(|c| c.max_telemetry.is_some()).unwrap().id.clone();
    assert_eq!(limits(&app, Screen::Motion, &effect, "max").map(|l| (l.0, l.1)), Some((-1800.0, 1800.0)));
    for (control, max) in [("height", 1200.0), ("surge_stroke", 250.0), ("actuator_distance_front_back", 3000.0)] {
        assert_eq!(limits(&app, Screen::Compensation, control, "").map(|l| (l.0, l.1)), Some((1.0, max)), "{control}");
    }
    // Rows that are no number boxes take the value as typed.
    assert!(number_box(&app, Screen::Motion, &effect, "").is_none());
    assert!(number_box(&app, Screen::Hardware, "brightness", "").is_none());
    assert!(number_box(&app, Screen::Pc, "search", "").is_none());
    drop(app.take_threads());
}

/// The text a number box shows for a stored value.
fn num(value: &serde_json::Value) -> String {
    crate::ui::html::num(components::number(value))
}

/// At start-up the stored LED count and segment count are put into their boxes again, so a count outside the
/// boxes' limits is stored within them.
#[test]
fn the_start_puts_the_stored_led_counts_within_their_limits() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["leds"]["nbr_leds"] = json!(500);
    backend.docs.lock().unwrap()["settings"]["leds"]["nbr_segments"] = json!(9);
    let mut app = hardware(&backend);
    assert_eq!(native(&backend, "LED", "nbr_leds"), LegacyValue::Int(150));
    assert_eq!(doc(&backend, &["settings", "leds", "nbr_segments"]), json!(4));
    drop(app.take_threads());
    // A segment count that is no whole number is left; the start goes on.
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["leds"]["nbr_segments"] = json!("many");
    backend.docs.lock().unwrap()["settings"]["leds"]["nbr_leds"] = json!(0);
    let mut app = hardware(&backend);
    assert_eq!(app.ml.fatal_error, None);
    assert_eq!(doc(&backend, &["settings", "leds", "nbr_segments"]), json!("many"));
    assert_eq!(native(&backend, "LED", "nbr_leds"), LegacyValue::Int(1));
    drop(app.take_threads());
}

/// A "2 TL" model puts the stored front activation threshold into its box again, within 0…1800.
#[test]
fn a_two_tl_model_puts_the_front_threshold_within_its_limits() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let model = crate::catalog::models().iter().find(|m| m.name.contains("2 TL")).unwrap().name.clone();
    backend.docs.lock().unwrap()["tune"]["min_traction_loss_front_activation"] = json!(4000);
    app.refresh_docs();
    app.set_motion_setting_native_checked("model", &LegacyValue::Str(model)).unwrap();
    model_rules(&mut app).unwrap();
    assert_eq!(tune(&backend, &["min_traction_loss_front_activation"]), json!(1800));
    drop(app.take_threads());
}

/// The rows the counting laser saw on the disk before each reading, and the file it looks at.
static SWEEP_FILE: Mutex<Option<String>> = Mutex::new(None);
static ROWS_ON_DISK: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// A laser reading that first counts the lines of the measurement file on the disk.
fn counting_laser() -> String {
    if let Some(path) = SWEEP_FILE.lock().unwrap().clone() {
        ROWS_ON_DISK.lock().unwrap().push(std::fs::read_to_string(path).unwrap().lines().count());
    }
    "12.5".to_owned()
}

const SHORT_SWEEP: SweepPlan = SweepPlan { values: &[0, 20], hold: Duration::ZERO, laser: counting_laser };

/// A measurement that writes all its rows: each row is on the disk before the next combination is measured, and
/// then every test slider goes back to 0 (not before: the sliders the measurement does not move keep theirs).
#[test]
fn a_finished_measurement_syncs_each_row_then_resets_the_test_sliders() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let folder = test_folder("sweep-finished");
    put(&backend, &["status", "documents"], json!(folder.display().to_string()));
    app.refresh_docs();
    for key in MOTION_TEST_KEYS {
        app.set_control_checked(&["tests", "motion_sliders", key], json!(9)).unwrap();
    }
    start_sweep(&mut app, SHORT_SWEEP).unwrap();
    let mut threads = app.take_threads();
    assert_eq!(threads.iter().map(|t| t.name).collect::<Vec<_>>(), ["motion_measurement"]);
    let path = app.ui.devices.hardware.sweep.as_ref().unwrap().path.clone();
    *SWEEP_FILE.lock().unwrap() = Some(path.clone());
    let shared = Arc::new(Mutex::new(app));
    (threads.remove(0).run)(Arc::downgrade(&shared));
    assert_eq!(*ROWS_ON_DISK.lock().unwrap(), (1..=8).collect::<Vec<_>>(), "each row is on the disk at once");
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written.lines().count(), 9, "{written}");
    assert!(written.lines().nth(8).unwrap().starts_with("20,20,20,") && written.ends_with(",12.5\n"));
    for key in MOTION_TEST_KEYS {
        assert_eq!(control(&backend, &["tests", "motion_sliders", key]), json!(0), "{key}");
    }
    let app = crate::host::lock(&shared);
    assert!(app.ui.devices.hardware.sweep.is_none());
    assert!(app.ui.notices.history.iter().any(|n| n.title == "Measurement"));
    std::fs::remove_dir_all(folder).unwrap();
}

/// Stop Test ends the measurement at once (its thread changes nothing more), the sliders go back to 0, and a new
/// start runs a new measurement, one at a time.
#[test]
fn stop_test_ends_the_measurement_and_a_new_start_runs_one_measurement() {
    let backend = FakeBackend::new();
    put(&backend, &["settings", "features", "motion_measurement_mode"], json!(true));
    let mut app = hardware(&backend);
    let folder = test_folder("sweep-stopped");
    put(&backend, &["status", "documents"], json!(folder.display().to_string()));
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
    assert_eq!(backend.patches("controls"), [json!({"tests": {"motion_test": true}})], "the sliders wait for the end");
    let first = app.ui.devices.hardware.sweep.clone().unwrap();
    let first_thread = app.take_threads().pop().unwrap();
    // Stop Test.
    for key in MOTION_TEST_KEYS {
        app.set_control_checked(&["tests", "motion_sliders", key], json!(9)).unwrap();
    }
    dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
    assert_eq!(control(&backend, &["tests", "motion_test"]), json!(false));
    assert!(first.stop.load(Ordering::SeqCst) && app.ui.devices.hardware.sweep.is_none());
    for key in MOTION_TEST_KEYS {
        assert_eq!(control(&backend, &["tests", "motion_sliders", key]), json!(0), "{key}");
    }
    assert!(app.take_threads().is_empty());
    // Start again: one new measurement.
    dispatch(&mut app, "action", "Hardware:motion_test:", "").unwrap();
    let second = app.ui.devices.hardware.sweep.clone().unwrap();
    assert!(!Arc::ptr_eq(&first.stop, &second.stop) && !second.stop.load(Ordering::SeqCst));
    let second_thread = app.take_threads();
    assert_eq!(second_thread.len(), 1);
    // The stopped measurement's thread, still winding down, moves no slider and writes no row.
    backend.calls.lock().unwrap().clear();
    let shared = Arc::new(Mutex::new(app));
    (first_thread.run)(Arc::downgrade(&shared));
    assert!(backend.patches("controls").is_empty());
    assert_eq!(std::fs::read_to_string(&first.path).unwrap().lines().count(), 1, "only the header");
    let mut app = crate::host::lock(&shared);
    assert!(app.ui.devices.hardware.sweep.as_ref().is_some_and(|s| Arc::ptr_eq(&s.stop, &second.stop)));
    // The progress row's Stop ends the measurement as well, the sliders back to 0.
    app.set_control_checked(&["tests", "motion_sliders", "yaw"], json!(40)).unwrap();
    dispatch(&mut app, "action", "Hardware:sweep_stop:", "").unwrap();
    assert!(second.stop.load(Ordering::SeqCst) && app.ui.devices.hardware.sweep.is_none());
    assert_eq!(control(&backend, &["tests", "motion_sliders", "yaw"]), json!(0));
    drop(second_thread);
    std::fs::remove_dir_all(folder).unwrap();
}

/// When the motion test cannot run (no model), its rules stop it, and a measurement with it.
#[test]
fn the_test_rules_end_a_measurement_when_the_test_cannot_run() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let stop = Arc::new(AtomicBool::new(false));
    app.ui.devices.hardware.sweep = Some(Sweep { row: 0, total: 8, path: String::new(), stop: stop.clone() });
    test_rules(&mut app).unwrap();
    assert!(stop.load(Ordering::SeqCst) && app.ui.devices.hardware.sweep.is_none());
    drop(app.take_threads());
}

/// A measurement whose row cannot be written reports the failure as a failed action does, shows no success and
/// leaves the test sliders where the measurement put them.
#[test]
fn a_failed_measurement_write_shows_its_error_and_no_success() {
    fn laser() -> String {
        "1".to_owned()
    }
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let folder = test_folder("sweep-failed");
    let path = folder.join("data.csv");
    std::fs::write(&path, "header\n").unwrap();
    // A file opened for reading only: its first row cannot be written.
    let file = std::fs::File::open(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let path = path.display().to_string();
    app.ui.devices.hardware.sweep = Some(Sweep { row: 0, total: 8, path: path.clone(), stop: stop.clone() });
    let shared = Arc::new(Mutex::new(app));
    let plan = SweepPlan { values: &[0, 20], hold: Duration::ZERO, laser };
    sweep(Arc::downgrade(&shared), file, stop, path, plan);
    let app = crate::host::lock(&shared);
    assert!(!app.ui.notices.history.iter().any(|n| n.title == "Measurement"), "no success");
    assert!(
        app.ui.notices.history.iter().any(|n| n.severity == notices::Severity::Warning
            && n.message.starts_with("An internal error occurred: OSError")),
        "the failure is reported"
    );
    assert!(app.ui.devices.hardware.sweep.is_none());
    assert_eq!(control(&backend, &["tests", "motion_sliders", "yaw"]), json!(0), "the first combination stays");
    drop(app);
    std::fs::remove_dir_all(folder).unwrap();
}

/// A LED count below a segment's start keeps the start (above the end then), on Hardware as at start.
#[test]
fn a_led_count_below_a_segment_start_keeps_the_start_as_at_start() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let set_segment = |backend: &FakeBackend| {
        let mut docs = backend.docs.lock().unwrap();
        docs["settings"]["leds"]["segments"][0]["start"] = json!(120);
        docs["settings"]["leds"]["segments"][0]["end"] = json!(150);
    };
    let segment =
        |backend: &FakeBackend| (native(backend, "LED", "segment1_start"), native(backend, "LED", "segment1_end"));
    set_segment(&backend);
    app.refresh_docs();
    dispatch(&mut app, "set", "Hardware:nbr_leds", "100").unwrap();
    assert_eq!(segment(&backend), (LegacyValue::Int(120), LegacyValue::Int(100)));
    // The start-up write-back of the segments stores the same.
    set_segment(&backend);
    app.refresh_docs();
    super::super::reload::start(&mut app).unwrap();
    assert_eq!(segment(&backend), (LegacyValue::Int(120), LegacyValue::Int(100)));
    drop(app.take_threads());
}

/// At start the belt calibration power and retraction, then (after the Motion refresh rate and the LED counts)
/// the iFlag brightness and the LED brightness are put again into their sliders: each is stored as a whole
/// number within 0…100.
#[test]
fn start_puts_the_belt_calibration_and_brightness_values_into_their_sliders_in_order() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    drop(app.take_threads());
    for (section, key, value) in [
        ("BELT_TENSIONER", "belt_calibration_factor", json!(140)),
        ("BELT_TENSIONER", "belt_calibration_retraction", json!(-5)),
        ("IFLAG", "brightness", json!(57.9)),
        ("LED", "brightness", json!("101")),
        ("MOTIONBOX", "refresh_rate", json!(5000)),
        ("LED", "nbr_segments", json!(9)),
    ] {
        app.backend.assign_setting_value(section, key, &LegacyValue::from(value)).unwrap();
    }
    app.refresh_docs();
    backend.calls.lock().unwrap().clear();
    start(&mut app).unwrap();
    assert_eq!(native(&backend, "BELT_TENSIONER", "belt_calibration_factor"), LegacyValue::Int(100));
    assert_eq!(native(&backend, "BELT_TENSIONER", "belt_calibration_retraction"), LegacyValue::Int(0));
    assert_eq!(native(&backend, "IFLAG", "brightness"), LegacyValue::Int(57));
    assert_eq!(native(&backend, "LED", "brightness"), LegacyValue::Int(100));
    assert_eq!(native(&backend, "MOTIONBOX", "refresh_rate"), LegacyValue::Int(1000));
    let patches = backend.patches("settings");
    let at = |found: &dyn Fn(&serde_json::Value) -> bool| patches.iter().position(found).unwrap();
    let power = at(&|p| p["belt"].get("calibration_factor").is_some());
    let retraction = at(&|p| p["belt"].get("calibration_retraction").is_some());
    let refresh = at(&|p| p["motion"].get("refresh_rate").is_some());
    let segments = at(&|p| p["leds"].get("nbr_segments").is_some());
    let leds = at(&|p| p["leds"].get("nbr_leds").is_some());
    let iflag = at(&|p| p["iflag"].get("brightness").is_some());
    let led = at(&|p| p["leds"].get("brightness").is_some());
    // The Motion refresh rate comes between the belt calibration and the LED settings.
    assert!(
        power < retraction
            && retraction < refresh
            && refresh < segments
            && segments < leds
            && leds < iflag
            && iflag < led,
        "{patches:?}"
    );
    drop(app.take_threads());
}

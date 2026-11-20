//! Tests of Hardware → Joystick: Status, the profiles, the rows and meters, the assignment dialog and its capture.

use std::collections::BTreeSet;
use std::sync::{Mutex, Weak};

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::Method;
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Theme};

fn joystick(backend: &std::sync::Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Joystick, 0);
    drop(app.take_threads());
    app
}

fn calls(backend: &FakeBackend, method: Method, path: &str) -> Vec<Option<Value>> {
    backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, p, _)| *m == method && p == path)
        .map(|(_, _, b)| b.clone())
        .collect()
}

fn assign(app: &MyApp) -> Assign {
    match dialogs_hardware::current(app) {
        Some(Shared::Joystick(Dialog::Assign(assign))) => assign.clone(),
        other => panic!("no assignment dialog: {other:?}"),
    }
}

/// Devices with one joystick "Pad" whose axis 2 is held at `value`.
fn devices(value: i64) -> Value {
    json!({"joysticks": {"guid": {"name": "Pad"}}, "state": {"Pad": {"axis": {"2": value}, "button": {"1": 1}}}})
}

#[test]
fn the_screen_shows_status_profiles_seven_rows_and_shortcuts_in_every_theme() {
    let backend = FakeBackend::new();
    let mut app = joystick(&backend);
    for theme in Theme::ALL {
        app.ui.theme = theme;
        let html = main(&app);
        assert!(html.contains("data-arg=\"Joystick:status\""), "{theme:?}");
        assert!(html.contains("id=\"joystick-profile\"") && html.contains(">Default</option>"));
        for (key, _) in logic::ASSIGNMENTS {
            assert!(html.contains(&format!("data-arg=\"Joystick:assign.{key}:\"")), "{key}");
        }
        assert!(html.contains("More settings · 3"), "four rows, then the rest");
        assert!(html.contains("Not configured") && html.contains("<meter"));
        assert!(html.contains("data-arg=\"Joystick:@header:0\""), "Actions → Shortcuts");
        // Delete is off for the default profile.
        assert!(html.contains("<button type=\"button\" disabled>Delete</button>"));
    }
    app.ui.theme = Theme::Vector;
    assert!(main(&app).contains("JOYSTICK"));
    dispatch(&mut app, "action", "Joystick:@header:0", "").unwrap();
    let Some(crate::ui::dialogs::Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("shortcut editor") };
    assert_eq!(editor.actions[0].0, "joystick_switch");
    drop(app.take_threads());
}

#[test]
fn status_sets_the_status_then_the_listener() {
    let backend = FakeBackend::new();
    let mut app = joystick(&backend);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Joystick:status", "1").unwrap();
    let patches = backend.patches("controls");
    assert_eq!(patches[0]["switches"]["joystick_status"], json!(1));
    assert_eq!(patches[1]["switches"]["joystick"], json!(true));
    app.refresh_docs();
    assert!(main(&app).contains("data-arg=\"Joystick:status\" checked"));
    dispatch(&mut app, "set", "Joystick:status", "0").unwrap();
    assert_eq!(backend.patches("controls").last().unwrap()["switches"]["joystick"], json!(false));
    drop(app.take_threads());
}

#[test]
fn profiles_are_created_copied_chosen_and_deleted() {
    let backend = FakeBackend::new();
    let mut app = joystick(&backend);
    // New…: an empty profile under the typed name, then selected.
    dispatch(&mut app, "action", "Joystick:profile-new:", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("maxlength=\"30\""));
    dispatch(&mut app, "dialog-field", "name", "Rally").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["joystick_db"]["Rally"], json!({}));
    assert_eq!(backend.docs.lock().unwrap()["joystick_profile"], json!("Rally"));
    assert!(app.ui.dialog.is_none());
    // A taken name keeps the dialog open with the reason.
    dispatch(&mut app, "action", "Joystick:profile-new:", "").unwrap();
    dispatch(&mut app, "dialog-field", "name", "Default").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains(logic::NAME_EXISTS));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    // Save as…: a copy under the proposed free name.
    backend.docs.lock().unwrap()["joystick_db"]["Rally"] = json!({"left": {"joystick": "Pad", "type": "button", "trigger": "1", "mode": "Single Axis", "invert_axis": false}});
    dispatch(&mut app, "action", "Joystick:profile-copy:", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("value=\"Rally (1)\""));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["joystick_db"]["Rally (1)"]["left"]["trigger"], json!("1"));
    assert!(main(&app).contains("Pad - button 1"));
    // Choosing a profile selects it; Delete asks, removes it and selects Default (which stays removable).
    dispatch(&mut app, "set", "Joystick:profile", "Rally").unwrap();
    assert_eq!(backend.docs.lock().unwrap()["joystick_profile"], json!("Rally"));
    dispatch(&mut app, "action", "Joystick:profile-delete:", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("Rally"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(calls(&backend, Method::Delete, "joystick/db").iter().any(|b| b.as_ref().unwrap()["key"] == "Rally"));
    assert_eq!(backend.docs.lock().unwrap()["joystick_profile"], json!("Default"));
    assert!(main(&app).contains("data-arg=\"Joystick:profile-delete:\""));
    drop(app.take_threads());
}

#[test]
fn an_unconfigured_row_captures_the_held_axis_and_apply_stores_it() {
    let backend = FakeBackend::new();
    let mut app = joystick(&backend);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", "Joystick:assign.left:", "").unwrap();
    assert!(!calls(&backend, Method::Delete, "joystick/devices/last-state").is_empty());
    assert_eq!(app.take_threads().len(), 1, "the capture runs on its own thread");
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("No joystick found") && html.contains(">Dual Axis</option>"));
    assert!(!html.contains("joystick-reverse"), "no reverse in dual mode");
    // The joystick and single mode are chosen: the instruction and the reverse switch show.
    let mut current = assign(&app);
    current.joysticks = vec!["Pad".into()];
    app.ui.dialog = Some(crate::ui::dialogs::Dialog::Hardware(Shared::Joystick(Dialog::Assign(current.clone()))));
    dispatch(&mut app, "dialog-field", "joystick", "Pad").unwrap();
    dispatch(&mut app, "dialog-field", "mode", logic::SINGLE_AXIS).unwrap();
    dispatch(&mut app, "dialog-field", "invert", "1").unwrap();
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("MOVE the axis") && html.contains("joystick-reverse"));
    // A pass with axis 2 held past 60 % fills Type and Axis; the same axis is then stuck.
    let generation = assign(&app).generation;
    let (mut capture, mut previous) = (logic::Capture::default(), None);
    capture_pass(&mut app, generation, &devices(30000), &mut capture, &mut previous);
    assert_eq!((assign(&app).kind.as_str(), assign(&app).trigger.as_str()), ("axis >", "2"));
    // A changed joystick list clears the capture.
    capture_pass(&mut app, generation, &json!({"joysticks": {}, "state": {}}), &mut capture, &mut previous);
    assert!(assign(&app).trigger.is_empty());
    assert_eq!(assign(&app).joystick, logic::NO_JOYSTICK_FOUND);
    // The joystick comes back: it is listed again and chosen, then its axis is held the other way.
    capture_pass(&mut app, generation, &devices(0), &mut capture, &mut previous);
    assert_eq!(assign(&app).joysticks, ["Pad"]);
    dispatch(&mut app, "dialog-field", "joystick", "Pad").unwrap();
    capture_pass(&mut app, generation, &devices(-30000), &mut capture, &mut previous);
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let stored = backend.docs.lock().unwrap()["joystick_db"]["Default"]["left"].clone();
    assert_eq!(
        stored,
        json!({"joystick": "Pad", "type": "axis <", "trigger": "2", "mode": "Single Axis", "invert_axis": true})
    );
    // The dialog is gone, so a capture thread still running stops.
    assert!(capturing(&app, generation).is_none());
    drop(app.take_threads());
}

#[test]
fn a_configured_row_shows_its_settings_and_capture_again_can_clear_it() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["joystick_db"]["Default"] = json!({"speed": {"joystick": "Pad", "type": "axis >", "trigger": "2", "mode": "Single Axis", "invert_axis": false}});
    let mut app = joystick(&backend);
    let html = main(&app);
    assert!(html.contains("Pad - single axis 2") && html.contains("Mode: Single Axis"));
    dispatch(&mut app, "action", "Joystick:assign.speed:", "").unwrap();
    assert!(app.take_threads().is_empty(), "no capture for a configured row");
    let html = crate::ui::render(&app, Region::Dialog);
    assert!(html.contains("Current Settings") && html.contains("Capture again") && html.contains(">axis &gt;<"));
    dispatch(&mut app, "dialog-button", "capture", "").unwrap();
    assert_eq!(app.take_threads().len(), 1);
    assert!(assign(&app).capturing && assign(&app).trigger.is_empty());
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.docs.lock().unwrap()["joystick_db"]["Default"].get("speed").is_none());
    drop(app.take_threads());
}

#[test]
fn meters_follow_the_assigned_inputs() {
    let assignments = json!({
        "left": {"joystick": "Pad", "type": "axis <", "trigger": "2", "mode": "Dual Axis", "invert_axis": false},
        "front": {"joystick": "Pad", "type": "axis >", "trigger": "2", "mode": "Single Axis", "invert_axis": true},
        "speed": {"joystick": "Pad", "type": "button", "trigger": "1", "mode": "Single Axis", "invert_axis": false},
    });
    let levels = levels(&assignments, &devices(-16384));
    assert_eq!(levels["left"], 50);
    assert_eq!(levels["front"], 75);
    assert_eq!(levels["speed"], 100);
}

#[test]
fn every_joystick_text_has_an_entry() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["joystick_db"]["Default"] = json!({"left": {"joystick": "Pad", "type": "axis >", "trigger": "2", "mode": "Dual Axis", "invert_axis": false}, "speed": {"joystick": "Pad", "type": "button", "trigger": "1", "mode": "Single Axis"}});
    let mut app = joystick(&backend);
    backend.translations.lock().unwrap().clear();
    app.ui.devices.joystick.error = Some("x".into());
    app.ui.devices.joystick.battery = Some(80.0);
    for theme in Theme::ALL {
        app.ui.theme = theme;
        main(&app);
    }
    let base = Assign {
        key: "left".into(),
        configured: false,
        joysticks: vec!["Pad".into()],
        joystick: "Pad".into(),
        mode: logic::SINGLE_AXIS.into(),
        invert: true,
        kind: "axis >".into(),
        trigger: "1".into(),
        capturing: true,
        generation: 1,
    };
    for dialog in [
        Dialog::Assign(base.clone()),
        Dialog::Assign(Assign { configured: true, ..base.clone() }),
        Dialog::Assign(Assign { joysticks: Vec::new(), joystick: logic::NO_JOYSTICK_FOUND.into(), ..base }),
        Dialog::Name { purpose: NameFor::New, name: String::new(), exists: true },
        Dialog::Name { purpose: NameFor::Copy, name: "x".into(), exists: false },
        Dialog::Remove { profile: "x".into() },
    ] {
        dialog_render(&app, &dialog);
    }
    let looked_up: BTreeSet<String> = backend.translations.lock().unwrap().iter().cloned().collect();
    let missing = crate::app::tests::texts_without_entry(&looked_up, &["x", "Pad"]);
    assert_eq!(missing, Vec::<String>::new());
    drop(app.take_threads());
}

/// A service with joystick devices; it can fail one call or one Status switch write, hold the first device read
/// and record whether the application was free at each device read.
struct Joy {
    inner: std::sync::Arc<FakeBackend>,
    devices: Mutex<Value>,
    fail: Mutex<Option<(Method, &'static str)>>,
    fail_switch: Mutex<Option<&'static str>>,
    app: Mutex<Weak<Mutex<MyApp>>>,
    reads: Mutex<Vec<bool>>,
    pause: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl Joy {
    fn new(inner: std::sync::Arc<FakeBackend>, devices: Value) -> std::sync::Arc<Joy> {
        std::sync::Arc::new(Joy {
            inner,
            devices: Mutex::new(devices),
            fail: Mutex::new(None),
            fail_switch: Mutex::new(None),
            app: Mutex::new(Weak::new()),
            reads: Mutex::new(Vec::new()),
            pause: Mutex::new(None),
        })
    }
}

impl crate::backend::Backend for Joy {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, crate::backend::Failure> {
        let failure = || crate::backend::Failure { status: 500, detail: "RuntimeError: fixture call failed".into() };
        if self.fail.lock().unwrap().is_some_and(|(m, p)| m == method && p == path) {
            self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
            return Err(failure());
        }
        if method == Method::Patch && path == "controls" {
            if let Some(key) = *self.fail_switch.lock().unwrap() {
                if body.as_ref().is_some_and(|b| b["switches"].get(key).is_some()) {
                    self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
                    return Err(failure());
                }
            }
        }
        if method == Method::Get && path == "joystick/devices" {
            let available = self.app.lock().unwrap().upgrade().is_some_and(|app| app.try_lock().is_ok());
            self.reads.lock().unwrap().push(available);
            if let Some((entered, release)) = self.pause.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            }
            self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
            return Ok(self.devices.lock().unwrap().clone());
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

/// The Joystick screen on the [`Joy`] service with `devices`.
fn joystick_with(devices: Value) -> (MyApp, std::sync::Arc<Joy>, std::sync::Arc<FakeBackend>) {
    let inner = FakeBackend::new();
    let mut app = joystick(&inner);
    let joy = Joy::new(inner.clone(), devices);
    app.backend = joy.clone();
    load(&mut app);
    (app, joy, inner)
}

/// Two joysticks, "Wheel" with axis 0 and "Pedals" with axis 1 held past 60 %.
fn two_devices() -> Value {
    json!({"joysticks": {"g1": {"name": "Wheel"}, "g2": {"name": "Pedals"}},
           "state": {"Wheel": {"axis": {"0": 22000}}, "Pedals": {"axis": {"1": -26000}}}})
}

#[test]
fn the_capture_reads_the_devices_without_holding_the_application() {
    let (mut app, joy, _) = joystick_with(devices(30000));
    dispatch(&mut app, "action", "Joystick:assign.front:", "").unwrap();
    drop(app.take_threads());
    dispatch(&mut app, "dialog-field", "joystick", "Pad").unwrap();
    let generation = assign(&app).generation;
    let (entered, waiting) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    *joy.pause.lock().unwrap() = Some((entered, released));
    joy.reads.lock().unwrap().clear();
    let shared = std::sync::Arc::new(Mutex::new(app));
    *joy.app.lock().unwrap() = std::sync::Arc::downgrade(&shared);
    let weak = std::sync::Arc::downgrade(&shared);
    let running = std::thread::spawn(move || capture_thread(weak, generation));
    // While a device read waits, the application can be used.
    waiting.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    {
        let mut app = shared.try_lock().expect("the application is free during the device read");
        dispatch(&mut app, "dialog-field", "mode", logic::SINGLE_AXIS).unwrap();
    }
    release.send(()).unwrap();
    let started = std::time::Instant::now();
    while joy.reads.lock().unwrap().len() < 3 {
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "the capture goes on");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let before = joy.reads.lock().unwrap().len();
    {
        let mut app = crate::host::lock(&shared);
        let current = assign(&app);
        assert_eq!(
            (current.kind.as_str(), current.trigger.as_str(), current.mode.as_str()),
            ("axis >", "2", logic::SINGLE_AXIS)
        );
        dispatch(&mut app, "dialog-close", "", "").unwrap();
    }
    running.join().unwrap();
    let reads = joy.reads.lock().unwrap();
    assert!(reads[..before].iter().all(|available| *available), "{reads:?}");
    drop(reads);
    drop(crate::host::lock(&shared).take_threads());
}

#[test]
fn a_failed_device_read_ends_the_capture_and_keeps_the_dialog() {
    let (mut app, joy, _) = joystick_with(devices(0));
    dispatch(&mut app, "action", "Joystick:assign.left:", "").unwrap();
    drop(app.take_threads());
    dispatch(&mut app, "dialog-field", "joystick", "Pad").unwrap();
    let before = assign(&app);
    let generation = before.generation;
    *joy.fail.lock().unwrap() = Some((Method::Get, "joystick/devices"));
    let shared = std::sync::Arc::new(Mutex::new(app));
    capture_thread(std::sync::Arc::downgrade(&shared), generation);
    let mut app = crate::host::lock(&shared);
    assert_eq!(assign(&app), before, "the dialog keeps what it showed");
    drop(app.take_threads());
}

#[test]
fn each_capture_pass_uses_the_joystick_chosen_in_the_dialog() {
    let (mut app, _, _) = joystick_with(two_devices());
    dispatch(&mut app, "action", "Joystick:assign.front:", "").unwrap();
    drop(app.take_threads());
    assert_eq!(assign(&app).joysticks, ["Wheel", "Pedals"]);
    let generation = assign(&app).generation;
    let (mut capture, mut previous) = (logic::Capture::default(), None);
    dispatch(&mut app, "dialog-field", "joystick", "Wheel").unwrap();
    capture_pass(&mut app, generation, &two_devices(), &mut capture, &mut previous);
    assert_eq!((assign(&app).kind.as_str(), assign(&app).trigger.as_str()), ("axis >", "0"));
    dispatch(&mut app, "dialog-field", "joystick", "Pedals").unwrap();
    capture_pass(&mut app, generation, &two_devices(), &mut capture, &mut previous);
    assert_eq!((assign(&app).kind.as_str(), assign(&app).trigger.as_str()), ("axis <", "1"));
    drop(app.take_threads());
}

#[test]
fn a_changed_or_re_enumerated_joystick_list_clears_the_capture() {
    let (mut app, _, _) = joystick_with(json!({"joysticks": {"g1": {"name": "Wheel"}}, "state": {}}));
    dispatch(&mut app, "action", "Joystick:assign.left:", "").unwrap();
    drop(app.take_threads());
    let generation = assign(&app).generation;
    let (mut capture, mut previous) = (logic::Capture::default(), None);
    let same = json!({"joysticks": {"g1": {"name": "Wheel"}}, "state": {}});
    capture_pass(&mut app, generation, &same, &mut capture, &mut previous);
    let mut current = assign(&app);
    current.kind = "axis >".into();
    current.trigger = "3".into();
    dispatch_hardware(&mut app, current);
    capture_pass(&mut app, generation, &same, &mut capture, &mut previous);
    assert_eq!(assign(&app).trigger, "3", "the same list keeps the capture");
    // The same joystick enumerated again (a new handle) is a changed list.
    let again = json!({"joysticks": {"g1": {"name": "Wheel", "joystick": 2}}, "state": {}});
    capture_pass(&mut app, generation, &again, &mut capture, &mut previous);
    assert!(assign(&app).kind.is_empty() && assign(&app).trigger.is_empty());
    assert_eq!(assign(&app).joysticks, ["Wheel"]);
    drop(app.take_threads());
}

/// Puts `assign` back as the open dialog.
fn dispatch_hardware(app: &mut MyApp, assign: Assign) {
    app.ui.dialog = Some(crate::ui::dialogs::Dialog::Hardware(Shared::Joystick(Dialog::Assign(assign))));
}

#[test]
fn a_failed_read_keeps_the_rows_and_shows_the_reason() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["joystick_db"]["Default"] = json!({"left": {"joystick": "Pad", "type": "button", "trigger": "1", "mode": "Single Axis", "invert_axis": false}});
    let mut app = joystick(&backend);
    let joy = Joy::new(backend.clone(), devices(0));
    app.backend = joy.clone();
    load(&mut app);
    assert!(main(&app).contains("Pad - button 1"));
    for (method, path) in
        [(Method::Get, "joystick/devices"), (Method::Get, "joystick/profile"), (Method::Get, "joystick/db")]
    {
        *joy.fail.lock().unwrap() = Some((method, path));
        load(&mut app);
        let html = main(&app);
        assert!(html.contains("Status unavailable: RuntimeError: fixture call failed"), "{path}");
        assert!(html.contains("Pad - button 1") && html.contains(">Default</option>"), "{path}: the rows stay");
    }
    *joy.fail.lock().unwrap() = None;
    load(&mut app);
    assert!(!main(&app).contains("Status unavailable"));
    // A profile table that is no table: the screen shows the reason instead of failing.
    backend.docs.lock().unwrap()["joystick_db"] = json!([]);
    load(&mut app);
    assert!(main(&app).contains("Status unavailable"));
    drop(app.take_threads());
}

#[test]
fn rows_show_their_assignments_and_the_meters_restart_when_they_change() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["joystick_db"]["Default"] = json!({
        "left": {"joystick": "Wheel", "type": "axis <", "trigger": "0", "mode": "Dual Axis", "invert_axis": false},
        "front": {"joystick": "Pedals", "type": "axis >", "trigger": "1", "mode": "Single Axis", "invert_axis": true},
        "speed": {"joystick": "Pedals", "type": "button", "trigger": "3", "mode": "Single Axis", "invert_axis": false},
    });
    let mut app = joystick(&backend);
    let joy = Joy::new(
        backend.clone(),
        json!({"joysticks": {"g1": {"name": "Wheel"}, "g2": {"name": "Pedals"}},
               "state": {"Wheel": {"axis": {"0": -16384}}, "Pedals": {"button": {"3": true}, "axis": {"1": -32768}}}}),
    );
    app.backend = joy.clone();
    load(&mut app);
    let html = main(&app);
    assert!(
        html.contains("Wheel - dual axis 0 &lt;")
            && html.contains("Pedals - single axis 1")
            && html.contains("Pedals - button 3")
    );
    assert!(html.contains("title=\"Joystick: Wheel\nType: axis &lt;\nAxis: 0\nMode: Dual Axis\""), "{html}");
    assert!(html.contains("<output id=\"joystick-left-level\">50</output>"));
    assert!(html.contains("<output id=\"joystick-front-level\">100</output>"), "inverted");
    assert!(html.contains("<output id=\"joystick-speed-level\">100</output>"));
    assert!(html.contains("<output id=\"joystick-right-level\">0</output>") && html.contains("Not configured"));
    // The main loop reads the rows again while the screen is open; changed assignments restart the meters.
    backend.docs.lock().unwrap()["joystick_db"]["Default"].as_object_mut().unwrap().shift_remove("speed");
    *joy.devices.lock().unwrap() = json!({"joysticks": {}, "state": {}});
    rules(&mut app).unwrap();
    let html = main(&app);
    assert!(html.contains("<output id=\"joystick-left-level\">0</output>"), "{html}");
    let speed = &html[html.find("data-control=\"speed\"").unwrap()..];
    assert!(speed.contains("Not configured"));
    drop(app.take_threads());
}

#[test]
fn an_unusable_trigger_has_no_meter_and_spaces_around_one_are_allowed() {
    let assignments = json!({
        "left": {"joystick": "Pad", "type": "axis >", "trigger": "x", "mode": "Dual Axis"},
        "front": {"joystick": "Pad", "type": "axis >", "trigger": " 2 ", "mode": "Single Axis", "invert_axis": false},
    });
    let levels = levels(&assignments, &devices(32767));
    assert!(!levels.contains_key("left"));
    assert_eq!(levels["front"], 100);
    // A single axis from its lowest to its highest value.
    assert_eq!(super::levels(&assignments, &devices(0))["front"], 50);
    assert_eq!(super::levels(&assignments, &devices(-32768))["front"], 0);
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["joystick_db"]["Default"] = assignments;
    let mut app = joystick(&backend);
    assert!(main(&app).contains("<output id=\"joystick-left-level\">0</output>"));
    drop(app.take_threads());
}

#[test]
fn the_joystick_shortcut_is_stored_from_the_screen_s_editor() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["settings"]["shortcuts"]["bindings"]["joystick_switch"] =
        json!({"keys": "", "buttons": ""});
    let mut app = joystick(&backend);
    dispatch(&mut app, "action", "Joystick:@header:0", "").unwrap();
    let Some(crate::ui::dialogs::Dialog::Shortcut(editor)) = &app.ui.dialog else { panic!("shortcut editor") };
    assert_eq!(editor.actions.len(), 1);
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(app.ui.dialog.is_none());
    let settings = backend.patches("settings");
    assert!(
        settings.iter().any(|p| p["shortcuts"]["bindings"]["joystick_switch"].get("keys").is_some()),
        "{settings:?}"
    );
    assert!(settings.iter().any(|p| p["shortcuts"]["bindings"]["joystick_switch"].get("buttons").is_some()));
    assert!(!calls(&backend, Method::Delete, "hotkeys/capture").is_empty(), "the capture ends");
    drop(app.take_threads());
}

#[test]
fn any_text_is_a_profile_name() {
    let backend = FakeBackend::new();
    let mut app = joystick(&backend);
    for name in ["", ".", ".."] {
        dispatch(&mut app, "action", "Joystick:profile-new:", "").unwrap();
        dispatch(&mut app, "dialog-field", "name", name).unwrap();
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert_eq!(backend.docs.lock().unwrap()["joystick_db"][name], json!({}), "{name:?} stored");
        assert_eq!(backend.docs.lock().unwrap()["joystick_profile"], json!(name));
        dispatch(&mut app, "action", "Joystick:profile-delete:", "").unwrap();
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        assert!(backend.docs.lock().unwrap()["joystick_db"].get(name).is_none(), "{name:?} removed");
    }
    // A long name is cut at 30 characters.
    dispatch(&mut app, "action", "Joystick:profile-new:", "").unwrap();
    dispatch(&mut app, "dialog-field", "name", &"n".repeat(40)).unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.docs.lock().unwrap()["joystick_db"].get("n".repeat(30)).is_some());
    drop(app.take_threads());
}

#[test]
fn a_copy_s_proposed_name_drops_brackets_and_trailing_spaces() {
    let backend = FakeBackend::new();
    backend.docs.lock().unwrap()["joystick_db"]["Wheel [x] \u{3000}"] = json!({});
    let mut app = joystick(&backend);
    dispatch(&mut app, "set", "Joystick:profile", "Wheel [x] \u{3000}").unwrap();
    dispatch(&mut app, "action", "Joystick:profile-copy:", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("value=\"Wheel (1)\""));
    dispatch(&mut app, "dialog-close", "", "").unwrap();
    dispatch(&mut app, "set", "Joystick:profile", "Default").unwrap();
    dispatch(&mut app, "action", "Joystick:profile-copy:", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("value=\"Default (1)\""));
    drop(app.take_threads());
}

#[test]
fn a_failing_status_write_stops_before_the_listener() {
    for failed in ["joystick_status", "joystick"] {
        let (mut app, joy, inner) = joystick_with(devices(0));
        *joy.fail_switch.lock().unwrap() = Some(failed);
        inner.calls.lock().unwrap().clear();
        assert_eq!(dispatch(&mut app, "set", "Joystick:status", "1").unwrap_err(), "RuntimeError: fixture call failed");
        let patches: Vec<Value> = calls(&inner, Method::Patch, "controls").into_iter().flatten().collect();
        assert_eq!(patches[0], json!({"switches": {"joystick_status": 1}}));
        let switches = inner.docs.lock().unwrap()["status"]["controls"]["switches"].clone();
        if failed == "joystick_status" {
            assert_eq!(patches.len(), 1, "the listener is not written");
            assert_eq!(switches["joystick_status"], json!(0));
        } else {
            assert_eq!(patches, [json!({"switches": {"joystick_status": 1}}), json!({"switches": {"joystick": true}})]);
            assert_eq!(switches["joystick_status"], json!(1));
        }
        assert_ne!(switches["joystick"], json!(true));
        *joy.fail_switch.lock().unwrap() = None;
        dispatch(&mut app, "set", "Joystick:status", "1").unwrap();
        let switches = inner.docs.lock().unwrap()["status"]["controls"]["switches"].clone();
        assert_eq!((switches["joystick_status"].clone(), switches["joystick"].clone()), (json!(1), json!(true)));
        drop(app.take_threads());
    }
}

#[test]
fn a_failing_profile_choice_or_removal_reports_the_error_and_keeps_the_profile() {
    let inner = FakeBackend::new();
    inner.docs.lock().unwrap()["joystick_db"]["Wheel"] = json!({});
    let mut app = joystick(&inner);
    let joy = Joy::new(inner.clone(), devices(0));
    app.backend = joy.clone();
    *joy.fail.lock().unwrap() = Some((Method::Put, "joystick/profile"));
    assert_eq!(
        dispatch(&mut app, "set", "Joystick:profile", "Wheel").unwrap_err(),
        "RuntimeError: fixture call failed"
    );
    assert_eq!(inner.docs.lock().unwrap()["joystick_profile"], json!("Default"));
    *joy.fail.lock().unwrap() = None;
    dispatch(&mut app, "set", "Joystick:profile", "Wheel").unwrap();
    assert_eq!(inner.docs.lock().unwrap()["joystick_profile"], json!("Wheel"));
    dispatch(&mut app, "action", "Joystick:profile-delete:", "").unwrap();
    *joy.fail.lock().unwrap() = Some((Method::Delete, "joystick/db"));
    assert_eq!(dispatch(&mut app, "dialog-ok", "", "").unwrap_err(), "RuntimeError: fixture call failed");
    assert!(app.ui.dialog.is_none(), "the question is closed");
    assert!(inner.docs.lock().unwrap()["joystick_db"].get("Wheel").is_some());
    assert_eq!(inner.docs.lock().unwrap()["joystick_profile"], json!("Wheel"));
    *joy.fail.lock().unwrap() = None;
    dispatch(&mut app, "action", "Joystick:profile-delete:", "").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(inner.docs.lock().unwrap()["joystick_db"].get("Wheel").is_none());
    assert_eq!(inner.docs.lock().unwrap()["joystick_profile"], json!("Default"));
    drop(app.take_threads());
}

#[test]
fn the_delete_question_names_the_selected_profile_and_cancel_keeps_it() {
    let backend = FakeBackend::new();
    for name in ["Wheel", "Pedals"] {
        backend.docs.lock().unwrap()["joystick_db"][name] = json!({});
    }
    let mut app = joystick(&backend);
    for name in ["Wheel", "Pedals", "Wheel"] {
        dispatch(&mut app, "set", "Joystick:profile", name).unwrap();
        dispatch(&mut app, "action", "Joystick:profile-delete:", "").unwrap();
        let html = crate::ui::render(&app, Region::Dialog);
        assert!(html.contains(&format!("{}{name}</p>", logic::REMOVE_QUESTION)), "{html}");
        dispatch(&mut app, "dialog-close", "", "").unwrap();
    }
    assert!(backend.docs.lock().unwrap()["joystick_db"].get("Wheel").is_some());
    assert!(backend.docs.lock().unwrap()["joystick_db"].get("Pedals").is_some());
    assert!(calls(&backend, Method::Delete, "joystick/db").is_empty());
    drop(app.take_threads());
}

#[test]
fn the_listener_follows_a_status_switched_elsewhere() {
    let backend = FakeBackend::new();
    let mut app = joystick(&backend);
    let switches = |backend: &FakeBackend| backend.docs.lock().unwrap()["status"]["controls"]["switches"].clone();
    rules(&mut app).unwrap();
    backend.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    assert!(calls(&backend, Method::Patch, "controls").is_empty(), "an unchanged status writes nothing");
    // A shortcut switched the joystick on: the status is stored as 1 and the listener follows it.
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["joystick_status"] = json!(true);
    rules(&mut app).unwrap();
    assert_eq!(
        calls(&backend, Method::Patch, "controls"),
        vec![Some(json!({"switches": {"joystick_status": 1}})), Some(json!({"switches": {"joystick": true}}))]
    );
    assert_eq!(switches(&backend)["joystick"], json!(true));
    // Off again, also while another screen is shown.
    navigate(&mut app, Screen::Hardware, 0);
    backend.calls.lock().unwrap().clear();
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["joystick_status"] = json!(0);
    rules(&mut app).unwrap();
    assert_eq!(
        calls(&backend, Method::Patch, "controls"),
        vec![Some(json!({"switches": {"joystick_status": 0}})), Some(json!({"switches": {"joystick": false}}))]
    );
    // The Status switch of the screen is no change from elsewhere.
    navigate(&mut app, Screen::Joystick, 0);
    dispatch(&mut app, "set", "Joystick:status", "1").unwrap();
    backend.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    assert!(calls(&backend, Method::Patch, "controls").is_empty());
    drop(app.take_threads());
}

/// "Save as…" proposes no name and opens nothing when the stored profiles cannot be read: the error ends it.
#[test]
fn save_as_stops_with_the_error_when_the_profiles_cannot_be_read() {
    let (mut app, joy, inner) = joystick_with(devices(0));
    *joy.fail.lock().unwrap() = Some((Method::Get, "joystick/db"));
    inner.calls.lock().unwrap().clear();
    assert_eq!(
        dispatch(&mut app, "action", "Joystick:profile-copy:", ""),
        Err("RuntimeError: fixture call failed".into())
    );
    assert!(app.ui.dialog.is_none());
    assert_eq!(calls(&inner, Method::Get, "joystick/db").len(), 1, "the first failed read ends the search");
    *joy.fail.lock().unwrap() = None;
    dispatch(&mut app, "action", "Joystick:profile-copy:", "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("value=\"Default (1)\""));
    drop(app.take_threads());
}

/// The live pass runs on its own thread from the start: every 0.1 s while the screen is open (the Windows rules
/// apply), every second otherwise; the main loop does not read the screen any more.
#[test]
fn the_live_pass_runs_on_its_own_thread_at_the_screen_period() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    let mut threads = app.take_threads();
    let at = threads.iter().position(|thread| thread.name == "joystick_meters").expect("the live pass thread");
    let live = threads.remove(at);
    drop(threads);
    navigate(&mut app, Screen::Joystick, 0);
    assert!(app.win32());
    assert_eq!(live_period(&app), Duration::from_millis(100));
    navigate(&mut app, Screen::Hardware, 0);
    assert_eq!(live_period(&app), Duration::from_secs(1));
    navigate(&mut app, Screen::Joystick, 0);
    drop(app.take_threads());
    let shared = std::sync::Arc::new(Mutex::new(app));
    backend.calls.lock().unwrap().clear();
    crate::ui::screens::hardware::main_loop_rules_shared(&shared).unwrap();
    assert!(calls(&backend, Method::Get, "joystick/devices").is_empty(), "the main loop leaves the meters alone");
    let weak = std::sync::Arc::downgrade(&shared);
    let running = std::thread::spawn(move || (live.run)(weak));
    let started = std::time::Instant::now();
    while calls(&backend, Method::Get, "joystick/devices").len() < 4 {
        assert!(started.elapsed() < Duration::from_secs(5), "the meters are read again and again");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(started.elapsed() < Duration::from_millis(1000), "four passes within a second");
    drop(crate::host::lock(&shared).take_threads());
    drop(shared);
    running.join().unwrap();
}

/// At the first pass the listener follows a stored status that is on (anything but false and 0), as the
/// Status switch, which starts off, does; a status that is off writes nothing.
#[test]
fn the_first_pass_turns_the_listener_on_for_a_status_that_is_on() {
    for (status, on) in [
        (json!(1), true),
        (json!(true), true),
        (json!("0"), true),
        (Value::Null, true),
        (json!(0), false),
        (json!(false), false),
        (json!(0.0), false),
    ] {
        let backend = FakeBackend::new();
        backend.docs.lock().unwrap()["status"]["controls"]["switches"]["joystick_status"] = status.clone();
        let mut app = loaded(backend.clone());
        let switches = |backend: &FakeBackend| -> Vec<Value> {
            calls(backend, Method::Patch, "controls")
                .into_iter()
                .flatten()
                .filter(|p| ["joystick_status", "joystick"].iter().any(|key| p["switches"].get(key).is_some()))
                .collect()
        };
        if on {
            assert_eq!(
                switches(&backend),
                [json!({"switches": {"joystick_status": 1}}), json!({"switches": {"joystick": true}})],
                "{status}"
            );
        } else {
            assert!(switches(&backend).is_empty(), "{status}");
        }
        backend.calls.lock().unwrap().clear();
        rules(&mut app).unwrap();
        assert!(switches(&backend).is_empty(), "{status}: only the first pass");
        drop(app.take_threads());
    }
}

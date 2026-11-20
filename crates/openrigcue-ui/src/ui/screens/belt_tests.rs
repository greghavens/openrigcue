//! Tests of Tuning → Belt: its writes, the calibration action and question, the calibration result, the hot
//! keys and the belt switch on a profile load.

use std::cell::RefCell;

use serde_json::json;

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dispatch, navigate, Region};

fn belt(backend: &std::sync::Arc<FakeBackend>) -> MyApp {
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("acc");
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Belt, 0);
    app
}

fn stored(backend: &FakeBackend, key: &str) -> serde_json::Value {
    backend.docs.lock().unwrap()["tune"][key].clone()
}

fn header_index(text: &str) -> usize {
    page().header.iter().position(|b| matches!(b, Widget::Button { text: t, .. } if t == text)).unwrap()
}

#[test]
fn every_belt_value_is_stored_as_a_whole_number() {
    let backend = FakeBackend::new();
    let mut app = belt(&backend);
    let html = main(&app);
    for group in ["Surge", "Sway", "Heave"] {
        assert!(html.contains(&format!(">{group}</option>")), "{group}");
    }
    for (arg, value, key, expected) in [
        ("Belt:belt_surge_enabled", "1", "belt_surge_enabled", json!(1)),
        ("Belt:belt_surge_reverse", "0", "belt_surge_reverse", json!(0)),
        ("Belt:belt_surge", "12", "belt_surge", json!(12)),
        ("Belt:belt_surge_smoothing", "40", "belt_surge_smoothing", json!(40)),
        ("Belt:belt_sway_reverse", "1", "belt_sway_curving_reverse", json!(1)),
        ("Belt:belt_curving", "9", "belt_sway_curving_factor", json!(9)),
        ("Belt:belt_heave", "3", "belt_heave_factor", json!(3)),
        ("Belt:belt_heave_smoothing", "70", "belt_heave_smoothing", json!(70)),
    ] {
        dispatch(&mut app, "set", arg, value).unwrap();
        assert_eq!(stored(&backend, key), expected, "{arg}");
    }
    drop(app.take_threads());
}

#[test]
fn calibrate_needs_a_connected_belt_and_asks_first() {
    let backend = FakeBackend::new();
    let mut app = belt(&backend);
    let calibrate = header_index("Calibrate");
    let html = main(&app);
    assert!(
        html.contains("Connect the belt to calibrate it.") && html.contains(&format!("Belt:@header:{calibrate}\""))
    );
    backend.docs.lock().unwrap()["status"]["accessories"]["belt"] = json!("Connected");
    backend.docs.lock().unwrap()["status"]["controls"]["switches"]["belt"] = json!(1);
    app.refresh_docs();
    assert!(!main(&app).contains("Connect the belt to calibrate it."));
    dispatch(&mut app, "action", &format!("Belt:@header:{calibrate}"), "").unwrap();
    assert!(crate::ui::render(&app, Region::Dialog).contains("Do you have the belt on?"));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "belt/homing"));
    drop(app.take_threads());
}

#[test]
fn the_calibration_result_waits_for_the_end_and_is_reported() {
    let reads = RefCell::new(vec![LegacyValue::Int(3), LegacyValue::Int(2), LegacyValue::Int(1), LegacyValue::Int(1)]);
    let mut sleeps = 0;
    let outcome = calibration_outcome(|| Ok(Some(reads.borrow_mut().remove(0))), |_| sleeps += 1);
    assert_eq!((outcome, sleeps), (Ok(Some(true)), 2));
    let backend = FakeBackend::new();
    let mut app = belt(&backend);
    report(&mut app, false);
    let notice = app.ui.notices.history.last().unwrap();
    assert_eq!((notice.title.as_str(), notice.message.as_str()), ("Failed", "Calibration did not succeed"));
    drop(app.take_threads());
}

#[test]
fn hot_keys_step_the_rows_and_a_profile_load_follows_the_game() {
    let backend = FakeBackend::new();
    let mut app = belt(&backend);
    dispatch(&mut app, "set", "Belt:belt_curving", "5").unwrap();
    dispatch(&mut app, "set", "Belt:belt_heave_smoothing", "0").unwrap();
    backend.docs.lock().unwrap()["ui_actions"] = json!(["belt_sway_curving_up", "belt_heave_smoothing_down"]);
    let pending = |action: &str| backend.ui_action_pending(action);
    assert!(apply_hotkeys(&mut app, &pending).unwrap());
    assert_eq!(stored(&backend, "belt_sway_curving_factor"), json!(6));
    assert_eq!(stored(&backend, "belt_heave_smoothing"), json!(0), "a step stays within the range");
    backend.docs.lock().unwrap()["status"]["session"]["game"] = json!("none");
    on_profile_load(&mut app).unwrap();
    assert_eq!(app.control(&["switches", "belt"]), &json!(false));
    drop(app.take_threads());
}

#[test]
fn the_belt_texts_have_entries() {
    let backend = FakeBackend::new();
    let mut app = belt(&backend);
    backend.translations.lock().unwrap().clear();
    main(&app);
    tuning::open_dialog(&mut app, Dialog::BeltCalibrate);
    crate::ui::render(&app, Region::Dialog);
    let looked_up = backend.translations.lock().unwrap().iter().cloned().collect();
    assert_eq!(crate::app::tests::texts_without_entry(&looked_up, &[]), Vec::<String>::new());
    drop(app.take_threads());
}

/// The opening tag of the element with `id` in `html`.
fn tag<'a>(html: &'a str, id: &str) -> &'a str {
    let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("no element {id}"));
    let start = html[..start].rfind('<').unwrap();
    let end = start + html[start..].find('>').unwrap();
    &html[start..=end]
}

#[test]
fn the_calibration_outcome_reads_again_once_the_calibration_ends() {
    let mut values = [3, 1, 0].into_iter().map(LegacyValue::Int);
    let mut waits = Vec::new();
    assert_eq!(calibration_outcome(|| Ok(values.next()), |duration| waits.push(duration)), Ok(Some(false)));
    assert_eq!(waits, [Duration::from_millis(300)]);
    assert_eq!(values.next(), None);
    for (values, expected, waits) in [
        (vec![LegacyValue::Int(0), LegacyValue::Int(1)], true, 0),
        (vec![LegacyValue::Str("2".into()), LegacyValue::Str("1".into())], false, 0),
        (vec![LegacyValue::Float(2.5), LegacyValue::Bool(true)], true, 0),
        (vec![LegacyValue::Float(3.0), LegacyValue::Int(0), LegacyValue::Bool(false)], false, 1),
    ] {
        let mut values = values.into_iter();
        let mut durations = Vec::new();
        assert_eq!(calibration_outcome(|| Ok(values.next()), |duration| durations.push(duration)), Ok(Some(expected)));
        assert_eq!(durations, vec![Duration::from_millis(300); waits]);
        assert_eq!(values.next(), None);
    }
    for prefix in [vec![], vec![LegacyValue::Int(0)], vec![LegacyValue::Int(3)]] {
        let mut values = prefix.into_iter();
        let mut calls = 0;
        let read = || {
            calls += 1;
            values.next().map(|value| Ok(Some(value))).unwrap_or_else(|| Err("KeyError: 'belt_homing'".to_owned()))
        };
        assert_eq!(calibration_outcome(read, |_| ()), Err("KeyError: 'belt_homing'".into()));
        assert!(calls <= 2);
    }
    assert_eq!(calibration_outcome(|| Ok(None), |_| panic!("no wait once the application is gone")), Ok(None));
}

/// A wrapper recording the reads of the calibration request, its request failing while `fail_request` is set.
struct CalibrationBackend {
    inner: std::sync::Arc<FakeBackend>,
    reads: std::sync::Mutex<Vec<&'static str>>,
    fail_request: std::sync::atomic::AtomicBool,
}

impl Backend for CalibrationBackend {
    fn call(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, Failure> {
        if path == "belt/homing" && self.fail_request.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Failure { status: 500, detail: "fixture homing failure".into() });
        }
        self.inner.call(method, path, body)
    }
    fn accessory_status(&self, name: &str) -> Result<LegacyValue, Failure> {
        self.reads.lock().unwrap().push("status");
        self.inner.accessory_status(name)
    }
    fn control_value(&self, path: &[&str]) -> Result<LegacyValue, Failure> {
        self.reads.lock().unwrap().push("switch");
        self.inner.control_value(path)
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

#[test]
fn the_calibration_reads_the_status_then_the_switch_and_needs_the_switch_exactly_on() {
    let inner = FakeBackend::new();
    let mut app = belt(&inner);
    let backend = std::sync::Arc::new(CalibrationBackend {
        inner: inner.clone(),
        reads: Default::default(),
        fail_request: std::sync::atomic::AtomicBool::new(true),
    });
    app.backend = backend.clone();
    inner.docs.lock().unwrap()["status"]["accessories"]["belt"] = json!("Not Connected");
    inner.docs.lock().unwrap()["status"]["controls"]["switches"].as_object_mut().unwrap().remove("belt");
    assert_eq!(request_calibration(&mut app), Ok(()));
    assert_eq!(*backend.reads.lock().unwrap(), ["status"], "a belt that is not connected is not asked further");
    inner.docs.lock().unwrap()["status"]["accessories"]["belt"] = json!("Connected");
    backend.reads.lock().unwrap().clear();
    assert_eq!(request_calibration(&mut app), Err("KeyError: 'belt'".into()));
    assert_eq!(*backend.reads.lock().unwrap(), ["status", "switch"]);
    for value in [json!(1), json!(1.0), json!(true)] {
        inner.docs.lock().unwrap()["status"]["controls"]["switches"]["belt"] = value.clone();
        assert_eq!(request_calibration(&mut app), Err("fixture homing failure".into()), "{value}");
    }
    for value in [json!(2), json!("yes"), json!("1"), json!(false)] {
        inner.docs.lock().unwrap()["status"]["controls"]["switches"]["belt"] = value.clone();
        assert_eq!(request_calibration(&mut app), Ok(()), "{value}");
    }
    assert!(inner.calls.lock().unwrap().iter().all(|(_, path, _)| path != "belt/homing"));
    // Through the question: switched on (true) the request goes out; 2 is not on.
    backend.fail_request.store(false, std::sync::atomic::Ordering::SeqCst);
    let calibrate = format!("Belt:@header:{}", header_index("Calibrate"));
    for (value, sent) in [(json!(true), true), (json!(2), false)] {
        inner.docs.lock().unwrap()["status"]["controls"]["switches"]["belt"] = value.clone();
        inner.calls.lock().unwrap().clear();
        dispatch(&mut app, "action", &calibrate, "").unwrap();
        dispatch(&mut app, "dialog-ok", "", "").unwrap();
        let posted = inner.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "belt/homing");
        assert_eq!(posted, sent, "{value}");
    }
    drop(app.take_threads());
}

#[test]
fn a_failing_belt_write_is_reported_and_nothing_is_stored() {
    let backend = FakeBackend::new();
    let mut app = belt(&backend);
    backend.docs.lock().unwrap()["tune"] = serde_json::Value::Null;
    backend.calls.lock().unwrap().clear();
    for (arg, value) in [("Belt:belt_surge", "7"), ("Belt:belt_surge_enabled", "1")] {
        let error = dispatch(&mut app, "set", arg, value).unwrap_err();
        assert!(error.starts_with("TypeError:"), "{arg}: {error}");
        assert_eq!(backend.docs.lock().unwrap()["tune"], serde_json::Value::Null);
    }
    assert!(backend.patches("profile").is_empty());
    backend.docs.lock().unwrap()["tune"] = app.docs.tune.clone();
    let before = stored(&backend, "belt_surge");
    assert!(dispatch(&mut app, "set", "Belt:belt_surge", "none").is_err());
    assert_eq!(stored(&backend, "belt_surge"), before);
    drop(app.take_threads());
}

#[test]
fn stored_belt_values_show() {
    let backend = FakeBackend::new();
    let mut app = belt(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["tune"]["belt_sway_curving_enabled"] = json!(1);
        docs["tune"]["belt_sway_curving_factor"] = json!(7);
        docs["tune"]["belt_surge"] = json!(9);
        docs["tune"]["belt_surge_enabled"] = json!(0);
    }
    app.refresh_docs();
    let html = main(&app);
    assert!(tag(&html, "c-belt-belt_surge").contains("value=\"9\""));
    assert!(!tag(&html, "c-belt-belt_surge_enabled").contains(" checked"));
    app.ui.section = 1;
    let html = main(&app);
    assert!(tag(&html, "c-belt-belt_sway_curving_enabled").contains(" checked"));
    assert!(tag(&html, "c-belt-belt_curving").contains("value=\"7\""));
    drop(app.take_threads());
}

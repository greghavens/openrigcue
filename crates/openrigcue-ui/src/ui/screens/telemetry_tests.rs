//! Tests of Games → Telemetry: its rows and actions, every value it stores, the live readings and history the
//! main loop reads, the reset, the snapshot, the shortcuts and the texts of every theme.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::app::tests::{loaded, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::{dialogs::Dialog as Open, dispatch, navigate, Region, Theme};

/// The fake service with the snapshot route.
struct SnapshotBackend {
    inner: Arc<FakeBackend>,
    answer: Mutex<Value>,
    /// The snapshot fails with this text instead.
    failure: Mutex<Option<String>>,
}

impl Backend for SnapshotBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if path == "telemetry/snapshot" {
            self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
            if let Some(detail) = self.failure.lock().unwrap().clone() {
                return Err(Failure { status: 500, detail });
            }
            return Ok(self.answer.lock().unwrap().clone());
        }
        self.inner.call(method, path, body)
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

fn telemetry(backend: &Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Telemetry, 0);
    drop(app.take_threads());
    backend.calls.lock().unwrap().clear();
    app
}

fn label(name: &str) -> &'static str {
    crate::catalog::channels().into_iter().find(|c| c.name == name).unwrap().label
}

fn displayed() -> crate::catalog::ChannelRow {
    crate::catalog::channels().into_iter().find(|c| c.display && c.name != "roll" && c.name != "pitch").unwrap()
}

/// Capture running with a game and a table of readings.
fn live(backend: &FakeBackend) {
    let mut docs = backend.docs.lock().unwrap();
    docs["status"]["session"]["game"] = json!("acc");
    docs["status"]["controls"]["telemetry"]["capture"] = json!(true);
    docs["telemetry_table"] = json!([
        {"label": label("roll"), "value": 1.5, "min": -2.25, "max": 3},
        {"label": displayed().label, "value": 12, "min": 0, "max": 40},
    ]);
    docs["telemetry_history"] = json!([0.0, 1.23456, 2.5, 1.0]);
}

#[test]
fn the_screen_offers_capture_export_status_plot_channels_and_actions() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    let html = main(&app);
    assert!(
        html.contains("data-arg=\"Telemetry:capture_telemetry\"")
            && html.contains("data-arg=\"Telemetry:export_telemetry\"")
    );
    assert!(html.contains(">Options<") && html.contains("data-arg=\"Telemetry:capture_telemetry:0\">Reset<"));
    assert!(html.contains("No live telemetry") && html.contains("id=\"telemetry-plot\""));
    assert!(html.contains(">Actions<") && html.contains(">Shortcuts<"));
    assert!(html.contains("title=\"Capture the current values and save them as CSV\" disabled>Save CSV Snapshot<"));
    assert!(html.contains("More settings · "), "the channels beyond four are under More settings");
    assert!(html.contains(&format!("data-channel=\"{}\"", displayed().name)));
    assert!(!html.contains("aria-label=\"History of the plotted channel\""), "no chart without live telemetry");
    drop(app.take_threads());
}

#[test]
fn every_telemetry_control_writes_through_the_service() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    dispatch(&mut app, "set", "Telemetry:capture_telemetry", "1").unwrap();
    let patches = backend.patches("controls");
    assert_eq!(patches[0]["telemetry"]["reset_min_max"], json!(false));
    assert_eq!(patches[1]["telemetry"]["capture"], json!(true));
    dispatch(&mut app, "set", "Telemetry:export_telemetry", "1").unwrap();
    assert!(backend.patches("controls").iter().any(|p| p["telemetry"]["export"] == json!(true)));
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "action", "Telemetry:capture_telemetry:0", "").unwrap();
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Post && p == "telemetry/reset"));
    assert!(backend.patches("controls").iter().any(|p| p["telemetry"]["reset_min_max"] == json!(true)));
    // The plotted channel: stored, and its history starts again.
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "set", "Telemetry:plot", "roll").unwrap();
    assert!(backend.patches("controls").iter().any(|p| p["telemetry"]["plot_channel"] == json!("roll")));
    assert!(backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "telemetry/history"));
    assert!(main(&app).contains("<option value=\"roll\" selected>"));
    assert!(dispatch(&mut app, "set", "Telemetry:plot", "nothing").is_err());
    drop(app.take_threads());
}

#[test]
fn the_main_loop_reads_live_values_and_history_while_capture_runs() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    rules(&mut app).unwrap();
    let state = &app.ui.devices.telemetry;
    assert!(state.active);
    let roll = &state.readings["roll"];
    assert_eq!((roll.value.as_str(), roll.min.as_str(), roll.max.as_str()), ("1.500000 →", "-2.250000", "3"));
    assert_eq!(state.readings[displayed().name].value, "12");
    assert_eq!(state.history.as_deref(), Some(&[0.0, 1.2346, 2.5, 1.0][..]));
    let html = main(&app);
    assert!(html.contains("Live telemetry") && html.contains("1.500000 →"));
    assert!(html.contains("aria-label=\"History of the plotted channel\"") && html.contains("<polyline"));
    assert!(html.contains("save them as CSV\">Save CSV Snapshot<"), "the snapshot is offered while live");
    // A history of zeros draws no chart; leaving the screen stops the readings.
    backend.docs.lock().unwrap()["telemetry_history"] = json!([0, 0]);
    rules(&mut app).unwrap();
    assert!(app.ui.devices.telemetry.history.is_none());
    navigate(&mut app, Screen::Pc, 0);
    backend.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    assert!(!backend.calls.lock().unwrap().iter().any(|(_, p, _)| p == "telemetry/table"));
    // A row the service reports an error for ends the pass.
    navigate(&mut app, Screen::Telemetry, 0);
    backend.docs.lock().unwrap()["telemetry_table"] = json!([{"label": label("roll"), "error": "ValueError: bad"}]);
    assert_eq!(rules(&mut app).unwrap_err(), "ValueError: bad");
    drop(app.take_threads());
}

#[test]
fn a_reset_while_capture_is_off_clears_the_minimum_and_maximum() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    rules(&mut app).unwrap();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["telemetry"]["capture"] = json!(false);
        docs["status"]["controls"]["telemetry"]["reset_min_max"] = json!(true);
    }
    backend.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    let roll = &app.ui.devices.telemetry.readings["roll"];
    assert!(roll.min.is_empty() && roll.max.is_empty() && !roll.value.is_empty());
    assert!(backend.patches("controls").iter().any(|p| p["telemetry"]["reset_min_max"] == json!(false)));
    assert!(!app.ui.devices.telemetry.active && app.ui.devices.telemetry.history.is_none());
    start(&mut app).unwrap();
    drop(app.take_threads());
}

#[test]
fn the_snapshot_reports_where_it_was_written_and_shortcuts_open_the_editor() {
    let inner = FakeBackend::new();
    let backend = Arc::new(SnapshotBackend {
        inner: inner.clone(),
        answer: Mutex::new(json!(["OK", "/home/u/telemetry.csv"])),
        failure: Mutex::new(None),
    });
    let mut app = started(backend.clone());
    navigate(&mut app, Screen::Telemetry, 0);
    let page = spec::telemetry_page();
    let at =
        |text: &str| page.header.iter().position(|b| matches!(b, Widget::Button { text: t, .. } if t == text)).unwrap();
    dispatch(&mut app, "action", &format!("Telemetry:@header:{}", at("Save CSV Snapshot")), "").unwrap();
    let last = app.ui.notices.history.last().unwrap();
    assert_eq!(
        (last.severity, last.message.as_str()),
        (Severity::Success, "Telemetry export written to /home/u/telemetry.csv")
    );
    *backend.answer.lock().unwrap() = json!(["ERROR", "No game is running"]);
    dispatch(&mut app, "action", &format!("Telemetry:@header:{}", at("Save CSV Snapshot")), "").unwrap();
    let last = app.ui.notices.history.last().unwrap();
    assert_eq!((last.severity, last.message.as_str()), (Severity::Warning, "No game is running"));
    *backend.answer.lock().unwrap() = Value::Null;
    assert!(dispatch(&mut app, "action", &format!("Telemetry:@header:{}", at("Save CSV Snapshot")), "").is_err());
    dispatch(&mut app, "action", &format!("Telemetry:@header:{}", at("Shortcuts")), "").unwrap();
    let Some(Open::Shortcut(editor)) = &app.ui.dialog else { panic!("shortcut editor") };
    let actions: Vec<&str> = editor.actions.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(actions, ["telemetry_capture", "telemetry_reset", "telemetry_snapshot", "telemetry_export"]);
    drop(app.take_threads());
}

#[test]
fn every_theme_draws_the_screen_and_every_text_has_an_entry() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    backend.translations.lock().unwrap().clear();
    for theme in Theme::ALL {
        app.ui.theme = theme;
        let html = crate::ui::render(&app, Region::Main);
        assert!(html.contains(if theme.capitals() { "TELEMETRY" } else { "Telemetry" }), "{theme:?}");
    }
    live(&backend);
    rules(&mut app).unwrap();
    app.ui.devices.telemetry.all_channels = true;
    for theme in Theme::ALL {
        app.ui.theme = theme;
        crate::ui::render(&app, Region::Main);
    }
    let looked_up: std::collections::BTreeSet<String> = backend.translations.lock().unwrap().iter().cloned().collect();
    let missing = crate::app::tests::texts_without_entry(&looked_up, &[]);
    assert_eq!(missing, Vec::<String>::new(), "texts without an entry in en.json");
    drop(app.take_threads());
}

/// The app after the page load and one main-loop pass.
fn started(backend: Arc<dyn Backend>) -> MyApp {
    let mut app = MyApp::new(backend);
    let body = app.gui.create("body", "BODY");
    let root = app.main(body);
    app.gui.set_root(root);
    crate::remi::gui::dispatch(&mut app, body, "onload", &Default::default());
    app.main_loop_pass();
    drop(app.take_threads());
    app
}

/// The fake service whose write of the capture switch fails.
struct CaptureFailureBackend {
    inner: Arc<FakeBackend>,
}

impl Backend for CaptureFailureBackend {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if method == Method::Patch
            && path == "controls"
            && body.as_ref().and_then(|body| body.pointer("/telemetry/capture")).is_some()
        {
            self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
            return Err(Failure { status: 500, detail: "RuntimeError: capture assignment failed".into() });
        }
        self.inner.call(method, path, body)
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

fn control(backend: &FakeBackend, key: &str) -> Value {
    backend.docs.lock().unwrap()["status"]["controls"]["telemetry"][key].clone()
}

fn history_reads(backend: &FakeBackend) -> usize {
    backend.calls.lock().unwrap().iter().filter(|(m, p, _)| *m == Method::Get && p == "telemetry/history").count()
}

/// The `action` argument of the header's Save CSV Snapshot.
fn snapshot_button() -> String {
    let page = spec::telemetry_page();
    let at = page
        .header
        .iter()
        .position(|b| matches!(b, Widget::Button { text, .. } if text == "Save CSV Snapshot"))
        .unwrap();
    format!("Telemetry:@header:{at}")
}

#[test]
fn a_failed_export_write_is_reported_and_leaves_the_stored_value() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    app.ui.devices.telemetry.history = Some(vec![1.0, 2.0]);
    backend.docs.lock().unwrap()["control_failure"] = json!("RuntimeError: export assignment failed");
    let error = dispatch(&mut app, "set", "Telemetry:export_telemetry", "1").unwrap_err();
    assert!(error.contains("RuntimeError: export assignment failed"), "{error}");
    assert_eq!(app.docs.controls["telemetry"]["export"], json!(false));
    assert_eq!(control(&backend, "export"), json!(false));
    assert_eq!(backend.patches("controls"), [json!({"telemetry": {"export": true}})]);
    backend.docs.lock().unwrap()["control_failure"] = Value::Null;
    dispatch(&mut app, "set", "Telemetry:export_telemetry", "1").unwrap();
    assert_eq!(app.docs.controls["telemetry"]["export"], json!(true));
    assert_eq!(control(&backend, "export"), json!(true));
    dispatch(&mut app, "set", "Telemetry:export_telemetry", "0").unwrap();
    assert_eq!(app.docs.controls["telemetry"]["export"], json!(false));
    assert_eq!(app.ui.devices.telemetry.history.as_deref(), Some(&[1.0, 2.0][..]), "Export leaves the chart");
    drop(app.take_threads());
}

#[test]
fn capture_clears_the_reset_flag_first_and_a_failed_write_keeps_the_chart() {
    // The capture write fails after the reset flag was cleared.
    let inner = FakeBackend::new();
    let mut app = started(Arc::new(CaptureFailureBackend { inner: inner.clone() }));
    navigate(&mut app, Screen::Telemetry, 0);
    inner.docs.lock().unwrap()["status"]["controls"]["telemetry"]["reset_min_max"] = json!(true);
    app.ui.devices.telemetry.history = Some(vec![1.0]);
    inner.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "set", "Telemetry:capture_telemetry", "1").unwrap_err();
    assert!(error.contains("RuntimeError: capture assignment failed"), "{error}");
    assert_eq!(control(&inner, "reset_min_max"), json!(false));
    assert_eq!(control(&inner, "capture"), json!(false));
    assert_eq!(app.docs.controls["telemetry"]["capture"], json!(false));
    assert_eq!(
        inner.patches("controls"),
        [json!({"telemetry": {"reset_min_max": false}}), json!({"telemetry": {"capture": true}})]
    );
    assert!(app.ui.devices.telemetry.history.is_some(), "the chart stays");
    drop(app.take_threads());
    // The reset write fails first: the capture is not written.
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    backend.docs.lock().unwrap()["status"]["controls"]["telemetry"]["reset_min_max"] = json!(true);
    backend.docs.lock().unwrap()["control_failure"] = json!("RuntimeError: reset assignment failed");
    app.ui.devices.telemetry.history = Some(vec![1.0]);
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "set", "Telemetry:capture_telemetry", "1").unwrap_err();
    assert!(error.contains("RuntimeError: reset assignment failed"), "{error}");
    assert_eq!(control(&backend, "reset_min_max"), json!(true));
    assert_eq!(backend.patches("controls"), [json!({"telemetry": {"reset_min_max": false}})]);
    assert!(app.ui.devices.telemetry.history.is_some());
    // Both writes succeed: the chart starts again.
    backend.docs.lock().unwrap()["control_failure"] = Value::Null;
    dispatch(&mut app, "set", "Telemetry:capture_telemetry", "1").unwrap();
    assert_eq!((control(&backend, "reset_min_max"), control(&backend, "capture")), (json!(false), json!(true)));
    assert!(app.ui.devices.telemetry.history.is_none());
    drop(app.take_threads());
}

#[test]
fn a_failed_snapshot_adds_no_notice_and_the_written_path_follows_the_translated_text() {
    let inner = FakeBackend::new();
    inner.docs.lock().unwrap()["translations"] =
        json!({"Telemetry export written to ": "translated:Telemetry export written to "});
    let backend = Arc::new(SnapshotBackend {
        inner: inner.clone(),
        answer: Mutex::new(json!(["OK", "/exports/snapshot.csv"])),
        failure: Mutex::new(None),
    });
    let mut app = started(backend.clone());
    navigate(&mut app, Screen::Telemetry, 0);
    let button = snapshot_button();
    dispatch(&mut app, "action", &button, "").unwrap();
    let last = app.ui.notices.history.last().unwrap();
    assert_eq!(
        (last.severity, last.message.as_str()),
        (Severity::Success, "translated:Telemetry export written to /exports/snapshot.csv")
    );
    *backend.answer.lock().unwrap() = json!(["ERROR", "Error: the telemetry could not be saved as CSV"]);
    dispatch(&mut app, "action", &button, "").unwrap();
    let last = app.ui.notices.history.last().unwrap();
    assert_eq!(
        (last.severity, last.message.as_str()),
        (Severity::Warning, "Error: the telemetry could not be saved as CSV")
    );
    let count = app.ui.notices.history.len();
    *backend.answer.lock().unwrap() = Value::Null;
    let error = dispatch(&mut app, "action", &button, "").unwrap_err();
    assert!(error.contains("TypeError: 'NoneType' object is not subscriptable"), "{error}");
    *backend.failure.lock().unwrap() = Some("RuntimeError: snapshot failed".into());
    let error = dispatch(&mut app, "action", &button, "").unwrap_err();
    assert!(error.contains("RuntimeError: snapshot failed"), "{error}");
    assert_eq!(app.ui.notices.history.len(), count, "a failed snapshot adds no result notice");
    drop(app.take_threads());
}

#[test]
fn every_channel_is_listed_while_the_service_shows_all_channels() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    let hidden = crate::catalog::channels().into_iter().find(|c| c.name == "max_rpm").unwrap();
    assert!(!hidden.display);
    let row = format!("data-channel=\"{}\"", hidden.name);
    assert!(!main(&app).contains(&row));
    live(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["show_all_channels"] = json!(true);
        docs["telemetry_table"] = json!([{"label": hidden.label, "value": "8000", "min": "7000", "max": "9000"}]);
    }
    rules(&mut app).unwrap();
    assert!(main(&app).contains(&row), "the flag is read again while the screen is open");
    let reading = &app.ui.devices.telemetry.readings[hidden.name];
    assert_eq!((reading.value.as_str(), reading.min.as_str(), reading.max.as_str()), ("8000", "7000", "9000"));
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["telemetry"]["capture"] = json!(false);
        docs["status"]["controls"]["telemetry"]["reset_min_max"] = json!(true);
    }
    rules(&mut app).unwrap();
    let reading = &app.ui.devices.telemetry.readings[hidden.name];
    assert_eq!((reading.value.as_str(), reading.min.as_str(), reading.max.as_str()), ("8000", "", ""));
    drop(app.take_threads());
}

#[test]
fn the_values_are_read_when_the_screen_opens() {
    let backend = FakeBackend::new();
    let mut app = loaded(backend.clone());
    live(&backend);
    navigate(&mut app, Screen::Telemetry, 0);
    assert_eq!(app.ui.devices.telemetry.readings["roll"].value, "1.500000 →");
    assert!(app.ui.devices.telemetry.history.is_some());
    drop(app.take_threads());
}

#[test]
fn a_row_error_keeps_the_readings_before_it_and_the_ones_after_as_they_were() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    let shown: Vec<_> = crate::catalog::channels().into_iter().filter(|c| c.display).collect();
    let (first, failing, after) = (&shown[0], &shown[1], &shown[2]);
    app.ui
        .devices
        .telemetry
        .readings
        .insert(after.name.to_owned(), Reading { value: "prior".into(), ..Reading::default() });
    live(&backend);
    backend.docs.lock().unwrap()["telemetry_table"] = json!([
        {"label": first.label, "value": "2", "min": "1", "max": "3", "direction_positive": true},
        {"label": failing.label, "error": "'measurement_unit'"},
        {"label": after.label, "value": "6", "min": "5", "max": "7", "direction_positive": true},
    ]);
    assert_eq!(rules(&mut app), Err("'measurement_unit'".into()));
    let readings = &app.ui.devices.telemetry.readings;
    assert!(readings[first.name].value.starts_with('2') && readings[first.name].min == "1", "{readings:?}");
    assert_eq!(readings[after.name].value, "prior");
    drop(app.take_threads());
}

#[test]
fn values_keep_their_text_and_roll_and_pitch_show_their_direction() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    backend.docs.lock().unwrap()["telemetry_table"] = json!([
        {"label": label("pitch"), "value": "True", "min": "False", "max": "True", "direction_positive": true},
        {"label": label("roll"), "value": "-0.000000", "min": "-0.000000", "max": "0", "direction_positive": false},
        {"label": label("gear"), "value": "3", "min": "1", "max": "3", "direction_positive": true},
    ]);
    rules(&mut app).unwrap();
    let value = |app: &MyApp, name: &str| app.ui.devices.telemetry.readings[name].value.clone();
    assert_eq!(value(&app, "pitch"), "True ↑");
    assert_eq!(value(&app, "roll"), "-0.000000 ←");
    assert_eq!(value(&app, "gear"), "3");
    // A value the service could not format leaves its value blank, arrow included; the rest shows.
    backend.docs.lock().unwrap()["telemetry_table"] = json!([
        {"label": label("pitch"), "value": "", "min": "-1", "max": "2", "direction_positive": true, "value_format_failed": true},
        {"label": label("roll"), "value": "-0.500000", "min": "-1", "max": "1", "direction_positive": false, "value_format_failed": false},
        {"label": label("gear"), "value": "3", "min": "1", "max": "3", "direction_positive": true},
    ]);
    rules(&mut app).unwrap();
    let pitch = &app.ui.devices.telemetry.readings["pitch"];
    assert_eq!((pitch.value.as_str(), pitch.min.as_str(), pitch.max.as_str()), ("", "-1", "2"));
    assert_eq!(value(&app, "roll"), "-0.500000 ←");
    assert_eq!(value(&app, "gear"), "3");
    drop(app.take_threads());
}

#[test]
fn a_direction_that_is_no_boolean_and_a_failed_reset_end_the_pass() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    backend.docs.lock().unwrap()["telemetry_table"] = json!([{"label": label("pitch"), "direction_positive": 3}]);
    assert_eq!(rules(&mut app), Err("invalid native attitude comparison".into()));
    app.ui
        .devices
        .telemetry
        .readings
        .insert("roll".into(), Reading { value: "1".into(), min: "0".into(), max: "2".into() });
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["telemetry"]["capture"] = json!(false);
        docs["status"]["controls"]["telemetry"]["reset_min_max"] = json!(true);
        docs["control_failure"] = json!("reset denied");
    }
    assert_eq!(rules(&mut app), Err("reset denied".into()));
    let roll = &app.ui.devices.telemetry.readings["roll"];
    assert!(roll.min.is_empty() && roll.max.is_empty(), "the minimum and maximum are cleared before the write");
    assert_eq!(control(&backend, "reset_min_max"), json!(true));
    drop(app.take_threads());
}

#[test]
fn the_reset_flag_is_left_alone_while_capture_runs() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    backend.docs.lock().unwrap()["status"]["controls"]["telemetry"]["reset_min_max"] = json!(true);
    backend.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    assert_eq!(control(&backend, "reset_min_max"), json!(true));
    assert!(backend.patches("controls").is_empty());
    drop(app.take_threads());
}

#[test]
fn the_plot_choice_stores_the_channel_name_in_any_language() {
    let backend = FakeBackend::new();
    let mut translations = serde_json::Map::new();
    translations.insert(label("rpm").to_owned(), json!("Drehzahl (übersetzt)"));
    backend.docs.lock().unwrap()["translations"] = Value::Object(translations);
    let mut app = telemetry(&backend);
    assert!(main(&app).contains("<option value=\"rpm\">Drehzahl (übersetzt)</option>"));
    dispatch(&mut app, "set", "Telemetry:plot", "rpm").unwrap();
    assert!(backend.patches("controls").iter().any(|p| p["telemetry"]["plot_channel"] == json!("rpm")));
    drop(app.take_threads());
}

#[test]
fn a_failed_plot_choice_keeps_the_history() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    app.ui.devices.telemetry.history = Some(vec![1.0, 2.0]);
    backend.docs.lock().unwrap()["telemetry_history"] = json!([1, 2]);
    backend.docs.lock().unwrap()["control_failure"] = json!("RuntimeError: plot assignment failed");
    backend.calls.lock().unwrap().clear();
    let error = dispatch(&mut app, "set", "Telemetry:plot", "rpm").unwrap_err();
    assert!(error.contains("RuntimeError: plot assignment failed"), "{error}");
    assert!(!backend.calls.lock().unwrap().iter().any(|(m, p, _)| *m == Method::Delete && p == "telemetry/history"));
    assert_eq!(backend.docs.lock().unwrap()["telemetry_history"], json!([1, 2]));
    assert!(app.ui.devices.telemetry.history.is_some());
    drop(app.take_threads());
}

#[test]
fn the_live_state_is_read_from_the_service_each_pass() {
    for (name, cached_capture, cached_game, capture, game, active) in [
        ("capture starts", false, "none", true, "api", true),
        ("capture stops", true, "api", false, "api", false),
        ("game stops", true, "api", true, "none", false),
    ] {
        let backend = FakeBackend::new();
        let mut app = telemetry(&backend);
        app.docs.controls["telemetry"]["capture"] = json!(cached_capture);
        app.docs.status["session"]["game"] = json!(cached_game);
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["status"]["controls"]["telemetry"]["capture"] = json!(capture);
            docs["status"]["session"]["game"] = json!(game);
            docs["telemetry_table"] = json!([{"label": label("pitch"), "value": 21, "min": 0, "max": 21}]);
            docs["telemetry_history"] = json!([1, 2]);
        }
        rules(&mut app).unwrap();
        let state = &app.ui.devices.telemetry;
        assert_eq!(state.active, active, "{name}");
        assert_eq!(state.readings.get("pitch").map(|r| r.value.as_str()), active.then_some("21 ↑"), "{name}");
        assert_eq!(state.history.is_some(), active, "{name}");
        drop(app.take_threads());
    }
}

#[test]
fn the_switches_follow_the_service_and_the_snapshot_is_offered_only_while_live() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["telemetry"]["capture"] = json!(true);
        docs["status"]["controls"]["telemetry"]["export"] = json!(true);
        docs["status"]["session"]["game"] = json!("api");
    }
    app.refresh_docs();
    rules(&mut app).unwrap();
    let html = main(&app);
    assert!(html.contains("data-arg=\"Telemetry:capture_telemetry\" checked"), "{html}");
    assert!(html.contains("data-arg=\"Telemetry:export_telemetry\" checked"));
    assert!(html.contains("save them as CSV\">Save CSV Snapshot<"));
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["telemetry"]["capture"] = json!(false);
        docs["status"]["controls"]["telemetry"]["export"] = json!(false);
    }
    app.refresh_docs();
    rules(&mut app).unwrap();
    let html = main(&app);
    assert!(!html.contains("data-arg=\"Telemetry:capture_telemetry\" checked"));
    assert!(!html.contains("data-arg=\"Telemetry:export_telemetry\" checked"));
    assert!(html.contains("save them as CSV\" disabled>Save CSV Snapshot<"));
    drop(app.take_threads());
}

#[test]
fn a_missing_game_ends_the_pass_while_capture_runs() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["controls"]["telemetry"]["capture"] = json!(true);
        docs["status"]["session"].as_object_mut().unwrap().remove("game");
    }
    let error = rules(&mut app).unwrap_err();
    assert!(error.starts_with("KeyError:"), "{error}");
    // Without capture the running game is not asked, so the same missing game ends nothing.
    backend.docs.lock().unwrap()["status"]["controls"]["telemetry"]["capture"] = json!(false);
    backend.calls.lock().unwrap().clear();
    rules(&mut app).unwrap();
    drop(app.take_threads());
}

#[test]
fn a_history_that_cannot_be_summed_keeps_the_chart_drawn_before() {
    for unusable in [json!(["12"]), json!([null]), json!([[]]), json!([{}])] {
        let backend = FakeBackend::new();
        let mut app = telemetry(&backend);
        live(&backend);
        backend.docs.lock().unwrap()["telemetry_history"] = json!([1, 3]);
        rules(&mut app).unwrap();
        assert_eq!(app.ui.devices.telemetry.history, Some(vec![1.0, 3.0]));
        backend.docs.lock().unwrap()["telemetry_history"] = unusable.clone();
        rules(&mut app).unwrap();
        assert_eq!(app.ui.devices.telemetry.history, Some(vec![1.0, 3.0]), "{unusable}");
        assert!(main(&app).contains("aria-label=\"History of the plotted channel\""), "{unusable}");
        drop(app.take_threads());
    }
}

/// The fake service with the telemetry table read held at a gate until the test releases it.
struct GatedTable {
    inner: Arc<FakeBackend>,
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Backend for GatedTable {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if method == Method::Get && path == "telemetry/table" {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                entered.send(()).unwrap();
                self.release.lock().unwrap().take().unwrap().recv().unwrap();
            }
        }
        self.inner.call(method, path, body)
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
fn the_main_loop_reads_the_table_without_holding_the_application() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    let (entered_send, entered) = std::sync::mpsc::channel();
    let (release, release_receive) = std::sync::mpsc::channel();
    app.backend = Arc::new(GatedTable {
        inner: backend.clone(),
        entered: Mutex::new(Some(entered_send)),
        release: Mutex::new(Some(release_receive)),
    });
    let shared = Arc::new(Mutex::new(app));
    let worker = {
        let shared = shared.clone();
        std::thread::spawn(move || rules_shared(&shared))
    };
    entered.recv().unwrap();
    assert!(shared.try_lock().is_ok(), "the application is not held while the table is read");
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    let mut app = Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap();
    assert!(app.ui.devices.telemetry.active && !app.ui.devices.telemetry.readings.is_empty());
    drop(app.take_threads());
}

#[test]
fn the_history_sum_keeps_integer_precision() {
    for (name, history, drawn) in [
        ("integer precision", json!([9007199254740993i64, -9007199254740992i64]), true),
        ("wide integer before floats", json!([10000000000000000i64, 1.0, -1e16]), false),
        ("wide integer cancellation before floats", json!([2147483648i64, -2147483648i64, 1e16, 1.0, -1e16]), true),
    ] {
        let backend = FakeBackend::new();
        let mut app = telemetry(&backend);
        live(&backend);
        backend.docs.lock().unwrap()["telemetry_history"] = history;
        rules(&mut app).unwrap();
        assert_eq!(app.ui.devices.telemetry.history.is_some(), drawn, "{name}");
        drop(app.take_threads());
    }
}

#[test]
fn the_history_is_read_once_per_pass_and_drawn_only_when_its_sum_is_not_zero() {
    for (sequence, expected) in [
        (json!([[1], [3, 4]]), Some(vec![1.0])),
        (json!([[0], [8]]), None),
        (json!([[1], "bad"]), Some(vec![1.0])),
        (json!([[1], []]), Some(vec![1.0])),
        (json!(["bad", [3]]), None),
        (json!([[10000000000000000i64, 1.0, -1e16], [5]]), None),
    ] {
        let backend = FakeBackend::new();
        let mut app = telemetry(&backend);
        live(&backend);
        backend.docs.lock().unwrap()["telemetry_history_sequence"] = sequence.clone();
        backend.calls.lock().unwrap().clear();
        rules(&mut app).unwrap();
        assert_eq!(history_reads(&backend), 1, "{sequence}");
        assert_eq!(app.ui.devices.telemetry.history, expected, "{sequence}");
        drop(app.take_threads());
    }
}

#[test]
fn a_history_summing_to_zero_or_a_stopped_capture_draws_no_chart() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    rules(&mut app).unwrap();
    assert!(main(&app).contains("aria-label=\"History of the plotted channel\""));
    backend.docs.lock().unwrap()["telemetry_history"] = json!([1, -1]);
    rules(&mut app).unwrap();
    assert!(app.ui.devices.telemetry.history.is_none());
    assert!(!main(&app).contains("aria-label=\"History of the plotted channel\""));
    backend.docs.lock().unwrap()["telemetry_history"] = json!([1]);
    backend.docs.lock().unwrap()["status"]["controls"]["telemetry"]["capture"] = json!(false);
    rules(&mut app).unwrap();
    assert!(app.ui.devices.telemetry.history.is_none());
    drop(app.take_threads());
}

#[test]
fn values_have_six_decimals_and_text_stays_as_it_is() {
    assert_eq!(format_value(&json!(1.5)), "1.500000");
    assert_eq!(format_value(&json!(3)), "3");
    assert_eq!(format_value(&Value::Null), "0");
    assert_eq!(format_value(&json!("3")), "3");
    assert_eq!(format_value(&json!("3.000000")), "3.000000");
    assert_eq!(format_value(&json!("[1, 2]")), "[1, 2]");
}

#[test]
fn a_row_error_ends_the_main_loop_before_the_history_is_read() {
    let backend = FakeBackend::new();
    let mut app = telemetry(&backend);
    live(&backend);
    backend.docs.lock().unwrap()["telemetry_table"] =
        json!([{"label": label("pitch"), "error": "injected native comparison failure"}]);
    backend.calls.lock().unwrap().clear();
    app.main_loop_pass();
    assert_eq!(app.ml.fatal_error.as_deref(), Some("injected native comparison failure"));
    assert_eq!(history_reads(&backend), 0);
    assert!(app.ui.devices.telemetry.history.is_none());
    drop(app.take_threads());
}

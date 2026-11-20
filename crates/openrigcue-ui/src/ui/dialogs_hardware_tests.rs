//! Tests of the Hardware → Hardware device dialogs and the device tests they run: the Thanos / PT-ACTUATOR
//! controller dialog with its Thanos4U settings and maximum-angle calculator, the PTA Motion System
//! destinations, the Axis Assignment, the speaker matrix, the output tests, the sound device choice, the manual
//! motion ports and the main loop's Hardware rules.

use std::sync::{Arc, Mutex};

use openrigcue_model::native_value::LegacyValue;
use serde_json::{json, Value};

use super::*;
use crate::app::tests::{alert_text, loaded, notice_texts, FakeBackend};
use crate::backend::{Backend, Failure, Method};
use crate::host::UiApp;
use crate::ui::screens::hardware as screen;
use crate::ui::{dispatch, navigate, Region, Screen};

// ---- Fixtures -------------------------------------------------------------------------------------------------

/// The Hardware screen, with the calls of the start-up left out of the record.
fn hardware(backend: &Arc<FakeBackend>) -> MyApp {
    let mut app = loaded(backend.clone());
    navigate(&mut app, Screen::Hardware, 0);
    drop(app.take_threads());
    backend.calls.lock().unwrap().clear();
    app
}

/// Shows a settings group (by its title) and returns the screen.
fn group(app: &mut MyApp, title: &str) -> String {
    let index = crate::spec::hardware_page().sections.iter().position(|s| s.title == title).unwrap();
    dispatch(app, "section", "", &index.to_string()).unwrap();
    screen::main(app)
}

fn dialog_html(app: &MyApp) -> String {
    crate::ui::render(app, Region::Dialog)
}

fn called(backend: &FakeBackend, method: Method, path: &str) -> Vec<Option<Value>> {
    backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, p, _)| *m == method && p == path)
        .map(|(.., b)| b.clone())
        .collect()
}

fn set(backend: &FakeBackend, path: &[&str], value: Value) {
    crate::backend::set_at(&mut backend.docs.lock().unwrap(), path, value);
}

fn remove(backend: &FakeBackend, path: &[&str], key: &str) {
    let mut docs = backend.docs.lock().unwrap();
    let parent = path.iter().fold(&mut *docs, |value, step| &mut value[*step]);
    parent.as_object_mut().unwrap().shift_remove(key);
}

fn doc(backend: &FakeBackend, path: &[&str]) -> Value {
    let docs = backend.docs.lock().unwrap();
    path.iter().fold(docs.clone(), |value, key| value[*key].clone())
}

/// The options shown in a select of the screen.
fn options(html: &str, id: &str) -> Vec<String> {
    let start = html.find(&format!("id=\"{id}\"")).unwrap_or_else(|| panic!("{id} in {html}"));
    let select = &html[start..start + html[start..].find("</select>").unwrap()];
    select
        .split("</option>")
        .filter_map(|part| part.rsplit('>').next())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
        .collect()
}

/// A service that fails one call, reads that one name, and records each guard read.
struct Probe {
    inner: Arc<FakeBackend>,
    fail_call: Option<(Method, &'static str, &'static str)>,
    fail_read: Option<&'static str>,
    reads: Mutex<Vec<String>>,
}

impl Probe {
    fn install(
        app: &mut MyApp,
        inner: &Arc<FakeBackend>,
        fail_call: Option<(Method, &'static str, &'static str)>,
    ) -> Arc<Probe> {
        let probe = Arc::new(Probe { inner: inner.clone(), fail_call, fail_read: None, reads: Mutex::new(Vec::new()) });
        app.backend = probe.clone();
        probe
    }

    fn failing_read(app: &mut MyApp, inner: &Arc<FakeBackend>, name: &'static str) -> Arc<Probe> {
        let probe = Arc::new(Probe {
            inner: inner.clone(),
            fail_call: None,
            fail_read: Some(name),
            reads: Mutex::new(Vec::new()),
        });
        app.backend = probe.clone();
        probe
    }

    fn read(&self, name: String) -> Result<(), Failure> {
        self.reads.lock().unwrap().push(name.clone());
        if self.fail_read == Some(name.as_str()) {
            return Err(Failure { status: 500, detail: format!("KeyError: '{name}'") });
        }
        Ok(())
    }
}

impl Backend for Probe {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        if let Some((failing, failing_path, detail)) = self.fail_call {
            if failing == method && failing_path == path {
                self.inner.calls.lock().unwrap().push((method, path.to_owned(), body));
                return Err(Failure { status: 500, detail: detail.into() });
            }
        }
        self.inner.call(method, path, body)
    }
    fn native_setting(&self, section: &str, key: &str) -> Result<LegacyValue, Failure> {
        self.read(format!("{section}.{key}"))?;
        self.inner.native_setting(section, key)
    }
    fn motion_parameter(&self, name: &str) -> Result<LegacyValue, Failure> {
        self.read(name.into())?;
        self.inner.motion_parameter(name)
    }
    fn accessory_status(&self, name: &str) -> Result<LegacyValue, Failure> {
        self.read(name.into())?;
        self.inner.accessory_status(name)
    }
    fn game_value(&self) -> Result<LegacyValue, Failure> {
        self.read("game".into())?;
        self.inner.game_value()
    }
    fn control_value(&self, path: &[&str]) -> Result<LegacyValue, Failure> {
        self.read(path.join("."))?;
        self.inner.control_value(path)
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
        self.inner.excluded_strings()
    }
}

/// The THANOS model with the most actuators.
fn thanos_model() -> &'static crate::catalog::ModelFlags {
    crate::catalog::models().iter().filter(|m| m.manufacturer == "THANOS").max_by_key(|m| m.port_mapping.len()).unwrap()
}

fn pt_model() -> &'static crate::catalog::ModelFlags {
    crate::catalog::models().iter().find(|m| m.manufacturer == "PT-ACTUATOR" && m.name != "PTA Motion System").unwrap()
}

/// A connected Thanos controller with its configuration (strokes of 15 cm), a distance of 558 mm and a stroke
/// of 150 mm, and Reverse on port 5 in the THANOS settings only.
fn thanos_backend(controller: &str) -> Arc<FakeBackend> {
    let backend = FakeBackend::new();
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["motion"]["link"] = json!({"status": "Connected", "ports": {"1": "/dev/ttyUSB0"}});
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
        for section in ["thanos", "pt_actuator"] {
            for port in 5..9 {
                docs["settings"][section][format!("reverse_controller_port_{port}")] =
                    json!(section == "thanos" && port == 5);
            }
            docs["settings"][section]["actuator_lateral_distance"] = json!(558.0);
            docs["settings"][section]["actuator_vertical_stroke"] = json!(150.0);
        }
    }
    backend
}

fn with_model(backend: &Arc<FakeBackend>, model: &str) -> MyApp {
    let mut app = hardware(backend);
    screen::choose_model(&mut app, model).unwrap();
    drop(app.take_threads());
    app
}

/// Controller Settings on the Motion model row.
fn open_controller(app: &mut MyApp) -> Result<(), String> {
    dispatch(app, "action", "Hardware:motion_model:2", "")
}

fn thanos(app: &MyApp) -> Thanos {
    match device(app) {
        Some(Device::Thanos(d)) => (**d).clone(),
        other => panic!("the controller dialog, not {other:?}"),
    }
}

fn thanos4u() -> (Arc<FakeBackend>, MyApp) {
    let backend = thanos_backend("Thanos4U(M)");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    assert_eq!(thanos(&app).thanos4u.as_ref().unwrap().strokes.len(), 8);
    (backend, app)
}

fn pending_calls(backend: &FakeBackend) -> Vec<Value> {
    called(backend, Method::Put, "motion/thanos/pending").into_iter().map(Option::unwrap).collect()
}

// ---- The Thanos / PT-ACTUATOR controller dialog -------------------------------------------------------------

#[test]
fn thanos_dialog_title_names_the_current_controller_and_reverse_flags_read_as_text() {
    let backend = thanos_backend("Thanos4U(S)");
    let mut app = with_model(&backend, &thanos_model().name);
    set(&backend, &["status", "devices", "thanos", "controller"], json!("Current"));
    set(&backend, &["status", "devices", "thanos", "firmware"], json!("17"));
    set(&backend, &["settings", "thanos", "reverse_controller_port_5"], json!("no"));
    open_controller(&mut app).unwrap();
    assert!(dialog_html(&app).contains("Thanos - Current - v17"));
    assert_eq!(thanos(&app).reverse[0], (5, false));
    assert!(thanos(&app).thanos4u.is_none(), "only a Thanos4U has the controller settings");
    drop(app.take_threads());
}

#[test]
fn thanos_dialog_does_not_open_on_an_unreadable_reverse_flag() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    set(&backend, &["settings", "thanos", "reverse_controller_port_7"], json!("broken"));
    assert_eq!(open_controller(&mut app).unwrap_err(), "Exception: Boolean value expected. Got broken");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn thanos_dialog_needs_a_known_configured_model() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    set(&backend, &["settings", "motion", "model"], json!("missing model"));
    assert_eq!(open_controller(&mut app).unwrap_err(), "KeyError: 'missing model'");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn controller_dialogs_read_the_reverse_flags_of_the_configured_manufacturer() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    assert_eq!(thanos(&app).reverse[0], (5, true), "THANOS settings");
    // PT-ACTUATOR: its own settings, the Can IDs and no calculator.
    screen::choose_model(&mut app, &pt_model().name).unwrap();
    open_controller(&mut app).unwrap();
    let d = thanos(&app);
    assert!(d.pt);
    assert_eq!(d.reverse[0], (5, false), "PT-ACTUATOR settings");
    let html = dialog_html(&app);
    assert!(html.contains(">PT-ACTUATOR<") && html.contains("Can ID") && !html.contains("MAX ANGLE"));
    drop(app.take_threads());
}

#[test]
fn thanos_dialog_names_main_and_secondary_ports_with_two_controllers() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    set(&backend, &["status", "motion", "link", "ports"], json!({"1": "COM7", "2": "COM8"}));
    open_controller(&mut app).unwrap();
    let html = dialog_html(&app);
    assert!(html.contains(">Main - 1<") && html.contains(">Secondary - 1<"));
    drop(app.take_threads());
}

#[test]
fn thanos_dialog_title_parts_must_be_text() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    set(&backend, &["status", "devices", "thanos", "firmware"], Value::Null);
    assert_eq!(open_controller(&mut app).unwrap_err(), "TypeError: can only concatenate str (not \"NoneType\") to str");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn thanos_dialog_needs_a_port_list() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    set(&backend, &["status", "motion", "link", "ports"], Value::Null);
    assert_eq!(open_controller(&mut app).unwrap_err(), "TypeError: object of type 'NoneType' has no len()");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

// ---- The maximum-angle calculator ----

#[test]
fn thanos_calculator_recommends_from_the_stored_distance_and_stroke() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    // 558 / 150 × 10 = 37°; 350 × 558 / 150 = 1302, times 10 for this controller.
    let d = thanos(&app);
    assert_eq!((d.lateral.as_str(), d.vertical.as_str(), d.max_angle, d.spike), ("558", "150", 37, 13020));
    let html = dialog_html(&app);
    assert!(html.contains("id=\"thanos-max-angle\">37<") && html.contains("id=\"thanos-spike\">13020<"));
    assert_eq!(slider_text(127, 1, 254), "49% 127");
    drop(app.take_threads());
}

#[test]
fn thanos_slider_text_is_bounded_and_survives_a_zero_span() {
    assert_eq!(slider_text(-253, 1, 254), "0% -253");
    assert_eq!(slider_text(507, 1, 254), "100% 507");
    assert_eq!(slider_text(1, 1, 1), "100% 1");
    assert_eq!(slider_text(0, 1, 1), "0% 0");
    assert_eq!(slider_text(i64::MIN, 1, 254), "0% -9223372036854775808");
    assert_eq!(slider_text(i64::MAX, i64::MAX - 1, i64::MAX), "100% 9223372036854775807");
}

#[test]
fn thanos_calculator_keeps_the_shown_value_for_text_that_is_no_number() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    // The box stays at the stroke it shows, which is stored again as a whole number.
    dispatch(&mut app, "dialog-field", "vertical", "broken").unwrap();
    assert_eq!(doc(&backend, &["settings", "pt_actuator", "actuator_vertical_stroke"]), json!(150));
    let d = thanos(&app);
    assert_eq!((d.vertical.as_str(), d.max_angle, d.spike), ("150", 37, 13020), "the dialog keeps its values");
    drop(app.take_threads());
}

#[test]
fn thanos_calculator_uses_the_current_controller_factor() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    set(&backend, &["status", "devices", "thanos", "controller"], json!("AMC-AASD15A"));
    dispatch(&mut app, "dialog-button", "calculate", "").unwrap();
    assert_eq!(thanos(&app).spike, 1302);
    drop(app.take_threads());
}

/// The calculator's boxes keep a typed value within 0…5000 (distance) and 0…500 (stroke) and store it as a
/// whole number.
#[test]
fn thanos_calculator_stores_typed_values_within_the_boxes_limits_as_whole_numbers() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    let html = dialog_html(&app);
    assert!(html.contains("data-arg=\"lateral\" min=\"0\" max=\"5000\" step=\"1\""));
    assert!(html.contains("data-arg=\"vertical\" min=\"0\" max=\"500\" step=\"1\""));
    for (field, key, typed, stored) in [
        ("lateral", "actuator_lateral_distance", "005", 5),
        ("lateral", "actuator_lateral_distance", "9000", 5000),
        ("lateral", "actuator_lateral_distance", "-3", 0),
        ("lateral", "actuator_lateral_distance", "558", 558),
        ("vertical", "actuator_vertical_stroke", "600", 500),
        ("vertical", "actuator_vertical_stroke", "150", 150),
    ] {
        dispatch(&mut app, "dialog-field", field, typed).unwrap();
        assert_eq!(doc(&backend, &["settings", "pt_actuator", key]), json!(stored), "{field} = {typed:?}");
        let d = thanos(&app);
        assert_eq!(if field == "lateral" { d.lateral } else { d.vertical }, stored.to_string());
    }
    assert_eq!((thanos(&app).max_angle, thanos(&app).spike), (37, 13020));
    // A fraction stays in the box; the recommendation needs whole numbers, so nothing is stored.
    assert!(dispatch(&mut app, "dialog-field", "vertical", "12.5").unwrap_err().starts_with("ValueError:"));
    assert_eq!(thanos(&app).vertical, "12.5");
    assert_eq!(doc(&backend, &["settings", "pt_actuator", "actuator_vertical_stroke"]), json!(150));
    drop(app.take_threads());
}

#[test]
fn thanos_calculator_store_failure_keeps_the_dialog() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    set(&backend, &["settings", "pt_actuator"], Value::Null);
    let error = dispatch(&mut app, "dialog-field", "lateral", "17").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert_eq!(doc(&backend, &["settings", "pt_actuator"]), Value::Null);
    // The box keeps what it took and the recommendation follows it (17 / 150 × 10 = 1°).
    let d = thanos(&app);
    assert_eq!((d.lateral.as_str(), d.max_angle), ("17", 1));
    drop(app.take_threads());
}

#[test]
fn thanos_calculator_caps_huge_distances() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    dispatch(&mut app, "dialog-field", "lateral", "9223372036854775807").unwrap();
    let d = thanos(&app);
    assert_eq!((d.max_angle, d.spike), (100, 64530));
    drop(app.take_threads());
}

#[test]
fn thanos_dialog_needs_a_numeric_stored_distance_and_stroke() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    set(&backend, &["settings", "thanos", "actuator_lateral_distance"], json!("broken"));
    let error = open_controller(&mut app).unwrap_err();
    assert!(error.starts_with("ValueError:"), "{error}");
    assert!(app.ui.dialog.is_none());
    set(&backend, &["settings", "thanos", "actuator_lateral_distance"], json!(558.0));
    remove(&backend, &["settings", "thanos"], "actuator_vertical_stroke");
    assert_eq!(open_controller(&mut app).unwrap_err(), "KeyError: 'actuator_vertical_stroke'");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn thanos_calculator_zero_stroke_skips_the_controller_and_accepts_underscores() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    remove(&backend, &["status", "devices", "thanos"], "controller");
    dispatch(&mut app, "dialog-field", "vertical", "0").unwrap();
    dispatch(&mut app, "dialog-field", "lateral", "broken").unwrap();
    let d = thanos(&app);
    assert_eq!((d.max_angle, d.spike), (0, 0));
    let html = dialog_html(&app);
    assert!(html.contains("data-arg=\"apply:angle\" disabled") && html.contains("data-arg=\"apply:spike\" disabled"));
    // The apply buttons do nothing while the recommendation is 0.
    dispatch(&mut app, "dialog-button", "apply:angle", "").unwrap();
    assert_eq!(thanos(&app).confirm, None);
    set(&backend, &["status", "devices", "thanos", "controller"], json!(""));
    dispatch(&mut app, "dialog-field", "lateral", "5_58").unwrap();
    dispatch(&mut app, "dialog-field", "vertical", "1_50").unwrap();
    let d = thanos(&app);
    assert_eq!((d.max_angle, d.spike), (37, 13020));
    drop(app.take_threads());
}

// ---- Apply to all games / vehicles ----

#[test]
fn apply_to_all_sends_the_recommended_spike_filter_and_reports_success() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    dispatch(&mut app, "dialog-button", "apply:spike", "").unwrap();
    assert!(dialog_html(&app).contains("Spike Filter Confirmation"));
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(
        called(&backend, Method::Post, "tunes/apply-all"),
        vec![Some(json!({"field": "spike_filter", "value": 13020}))]
    );
    assert!(notice_texts(&app).iter().any(|(_, m)| m == "Every game / vehicle now uses the new spike filter"));
    assert_eq!(thanos(&app).confirm, None, "back in the controller dialog");
    drop(app.take_threads());
}

#[test]
fn apply_to_all_failure_shows_no_success_notice() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    dispatch(&mut app, "dialog-button", "apply:angle", "").unwrap();
    Probe::install(
        &mut app,
        &backend,
        Some((Method::Post, "tunes/apply-all", "OperationalError: fixture commit failed")),
    );
    assert_eq!(dispatch(&mut app, "dialog-ok", "", "").unwrap_err(), "OperationalError: fixture commit failed");
    assert_eq!(
        called(&backend, Method::Post, "tunes/apply-all"),
        vec![Some(json!({"field": "max_angle", "value": 37}))]
    );
    assert!(!notice_texts(&app).iter().any(|(_, m)| m.starts_with("Every game / vehicle")));
    assert_eq!(thanos(&app).confirm, Some(false), "the confirmation stays open");
    drop(app.take_threads());
}

// ---- OK ----

#[test]
fn thanos_ok_publishes_the_pending_settings_before_needing_the_model() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    set(&backend, &["settings", "motion", "model"], json!("missing model"));
    assert_eq!(dispatch(&mut app, "dialog-ok", "", "").unwrap_err(), "KeyError: 'missing model'");
    let pending = pending_calls(&backend);
    assert_eq!(pending.len(), 2);
    assert_eq!((pending[0]["secondary"].clone(), pending[0]["replace"].clone()), (json!(false), json!(true)));
    assert_eq!((pending[1]["secondary"].clone(), pending[1]["replace"].clone()), (json!(true), json!(true)));
    drop(app.take_threads());
}

#[test]
fn thanos_ok_with_two_controllers_stores_the_main_count_before_needing_the_model() {
    let backend = thanos_backend("");
    set(&backend, &["status", "motion", "link", "ports"], json!({"1": "COM7", "2": "COM8"}));
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    set(&backend, &["settings", "motion", "model"], json!("missing model"));
    assert_eq!(dispatch(&mut app, "dialog-ok", "", "").unwrap_err(), "KeyError: 'missing model'");
    let pending = pending_calls(&backend);
    assert_eq!(pending[2..], [json!({"secondary": false, "replace": false, "entries": [["motor_number", 4]]})]);
    drop(app.take_threads());
}

#[test]
fn thanos_ok_splits_the_motor_count_between_two_controllers() {
    let backend = thanos_backend("");
    set(&backend, &["status", "motion", "link", "ports"], json!({"1": "COM7", "2": "COM8"}));
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let pending = pending_calls(&backend);
    let motors = i64::from(thanos_model().motors);
    assert_eq!(
        pending[2..],
        [
            json!({"secondary": false, "replace": false, "entries": [["motor_number", 4]]}),
            json!({"secondary": true, "replace": false, "entries": [["motor_number", motors - 4]]}),
        ]
    );
    drop(app.take_threads());
}

#[test]
fn thanos_ok_reverse_write_needs_the_settings_section() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    set(&backend, &["settings", "thanos"], Value::Null);
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert!(pending_calls(&backend).iter().any(|body| body["entries"][0][0] == json!("motor_number")));
    assert_eq!(doc(&backend, &["settings", "thanos"]), Value::Null);
    drop(app.take_threads());
}

#[test]
fn thanos_ok_reads_the_model_again_for_each_reverse_write() {
    let backend = thanos_backend("");
    let model = thanos_model().name.clone();
    let mut app = with_model(&backend, &model);
    open_controller(&mut app).unwrap();
    dispatch(&mut app, "dialog-field", "reverse:5", "0").unwrap();
    backend.calls.lock().unwrap().clear();
    // The motor count, the write of port 5, then a model that is gone.
    set(&backend, &["native_setting_sequence", "MOTIONBOX", "model"], json!([model, model, "missing model"]));
    assert_eq!(dispatch(&mut app, "dialog-ok", "", "").unwrap_err(), "KeyError: 'missing model'");
    assert_eq!(backend.patches("settings"), vec![json!({"thanos": {"reverse_controller_port_5": false}})]);
    assert_eq!(doc(&backend, &["settings", "thanos", "reverse_controller_port_5"]), json!(false));
    drop(app.take_threads());
}

#[test]
fn thanos_ok_needs_the_port_list_before_the_motor_counts() {
    let backend = thanos_backend("");
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    set(&backend, &["status", "motion", "link", "ports"], Value::Null);
    assert_eq!(
        dispatch(&mut app, "dialog-ok", "", "").unwrap_err(),
        "TypeError: object of type 'NoneType' has no len()"
    );
    assert_eq!(pending_calls(&backend).len(), 2, "only the two pending configurations");
    drop(app.take_threads());
}

// ---- The Thanos4U settings ----

const THANOS_SLIDER_KEYS: [&str; 8] = [
    "park_position",
    "6dof_start_pos",
    "spike_move_speed",
    "spike_fine_move_speed",
    "park_move_speed",
    "start_move_step",
    "start_move_timeout",
    "park_move_timeout",
];

#[test]
fn thanos4u_values_must_be_numbers() {
    let (_, mut app) = thanos4u();
    for key in THANOS_SLIDER_KEYS {
        let error = dispatch(&mut app, "dialog-field", &format!("slider:{key}"), "NaN").unwrap_err();
        assert!(error.starts_with("ValueError:"), "{key}: {error}");
    }
    let error = dispatch(&mut app, "dialog-field", "stroke:1", "NaN").unwrap_err();
    assert!(error.starts_with("ValueError:"), "{error}");
    let d = thanos(&app);
    assert!(d.pending.is_empty() && d.pending2.is_empty());
    drop(app.take_threads());
}

#[test]
fn thanos4u_sliders_are_pending_on_both_controllers() {
    let (_, mut app) = thanos4u();
    for key in THANOS_SLIDER_KEYS {
        dispatch(&mut app, "dialog-field", &format!("slider:{key}"), "17").unwrap();
        let d = thanos(&app);
        assert!(d.pending.contains(&(key.to_owned(), 17)) && d.pending2.contains(&(key.to_owned(), 17)), "{key}");
    }
    let html = dialog_html(&app);
    assert!(html.contains("id=\"thanos-park_move_timeout-out\" for=\"thanos-park_move_timeout\">17s<"));
    assert!(html.contains(&format!(
        "id=\"thanos-park_position-out\" for=\"thanos-park_position\">{}<",
        slider_text(17, 1, 254)
    )));
    drop(app.take_threads());
}

#[test]
fn thanos4u_strokes_go_to_the_controller_of_their_port() {
    let (_, mut app) = thanos4u();
    dispatch(&mut app, "dialog-field", "stroke:1", "17").unwrap();
    dispatch(&mut app, "dialog-field", "stroke:5", "17").unwrap();
    let d = thanos(&app);
    assert_eq!(d.pending, vec![("stroke1".to_owned(), 1)]);
    assert_eq!(d.pending2, vec![("stroke1".to_owned(), 1)]);
    let html = dialog_html(&app);
    for port in [1, 5] {
        assert!(html.contains(&format!("id=\"thanos-stroke-{port}-out\" for=\"thanos-stroke-{port}\">17mm<")));
    }
    drop(app.take_threads());
}

#[test]
fn thanos4u_choices_outside_the_list_are_refused() {
    let (_, mut app) = thanos4u();
    for value in ["9", "missing"] {
        let error = dispatch(&mut app, "dialog-field", "var:1:lead_screw", value).unwrap_err();
        assert!(error.starts_with("ValueError:"), "{error}");
        let error = dispatch(&mut app, "dialog-field", "mode:estop_mode", value).unwrap_err();
        assert!(error.starts_with("ValueError:"), "{error}");
    }
    let d = thanos(&app);
    assert!(d.pending.is_empty() && d.pending2.is_empty());
    drop(app.take_threads());
}

#[test]
fn thanos4u_choices_with_two_controllers_go_to_the_secondary_configuration() {
    let backend = thanos_backend("Thanos4U(M)");
    set(&backend, &["status", "motion", "link", "ports"], json!({"1": "COM7", "2": "COM8"}));
    let mut app = with_model(&backend, &thanos_model().name);
    open_controller(&mut app).unwrap();
    // 10mm/rev (map key 3) on actuator 1; Hold Powered (map key 1).
    dispatch(&mut app, "dialog-field", "var:1:lead_screw", "2").unwrap();
    let d = thanos(&app);
    assert!(d.pending.is_empty());
    assert_eq!(d.pending2, vec![("lead_screw-3".to_owned(), 3)]);
    dispatch(&mut app, "dialog-field", "mode:estop_mode", "0").unwrap();
    assert_eq!(thanos(&app).pending2, vec![("lead_screw-3".to_owned(), 3), ("estop_mode".to_owned(), 1)]);
    drop(app.take_threads());
}

#[test]
fn thanos4u_choice_needs_the_port_list_after_checking_the_choice() {
    let (backend, mut app) = thanos4u();
    set(&backend, &["status", "motion", "link", "ports"], Value::Null);
    let error = dispatch(&mut app, "dialog-field", "var:1:lead_screw", "x").unwrap_err();
    assert!(error.starts_with("ValueError:"), "the choice is checked first: {error}");
    assert_eq!(
        dispatch(&mut app, "dialog-field", "var:1:lead_screw", "2").unwrap_err(),
        "TypeError: object of type 'NoneType' has no len()"
    );
    let d = thanos(&app);
    assert!(d.pending.is_empty() && d.pending2.is_empty());
    drop(app.take_threads());
}

#[test]
fn thanos4u_choices_keep_the_pending_order() {
    let (_, mut app) = thanos4u();
    for (field, index) in [("var:1:lead_screw", "0"), ("var:1:inline", "1"), ("var:1:lead_screw", "1")] {
        dispatch(&mut app, "dialog-field", field, index).unwrap();
    }
    let d = thanos(&app);
    assert_eq!(d.pending, vec![("lead_screw1".to_owned(), 1), ("inline1".to_owned(), 2)]);
    assert!(d.pending2.is_empty());
    drop(app.take_threads());
}

#[test]
fn thanos4u_dialog_shows_the_controllers_current_values() {
    let (backend, mut app) = thanos4u();
    set(&backend, &["status", "devices", "thanos", "config", "stroke1"], json!(20));
    set(&backend, &["status", "devices", "thanos", "config2", "stroke1"], json!(30));
    open_controller(&mut app).unwrap();
    let strokes = thanos(&app).thanos4u.unwrap().strokes;
    assert_eq!((strokes[0], strokes[4]), ((1, 200), (5, 300)));
    let html = dialog_html(&app);
    assert!(html.contains("id=\"thanos-stroke-1-out\" for=\"thanos-stroke-1\">200mm<"));
    assert!(html.contains("id=\"thanos-stroke-5-out\" for=\"thanos-stroke-5\">300mm<"));
    drop(app.take_threads());
}

#[test]
fn thanos4u_dialog_reads_text_and_fractional_numbers_as_whole_numbers() {
    let (backend, mut app) = thanos4u();
    for (key, value, expected) in [("stroke1", json!("15"), 150), ("stroke1", json!(15.5), 150)] {
        set(&backend, &["status", "devices", "thanos", "config", key], value);
        open_controller(&mut app).unwrap();
        assert_eq!(thanos(&app).thanos4u.unwrap().strokes[0], (1, expected));
    }
    for (value, expected) in [(json!(1.5), 1), (json!("5"), 5)] {
        set(&backend, &["status", "devices", "thanos", "config", "park_move_timeout"], value);
        open_controller(&mut app).unwrap();
        let sliders = thanos(&app).thanos4u.unwrap().sliders;
        assert!(sliders.contains(&("park_move_timeout", expected)), "{sliders:?}");
    }
    drop(app.take_threads());
}

#[test]
fn thanos4u_dialog_refuses_values_outside_its_maps() {
    let (backend, mut app) = thanos4u();
    let lead_screw = ["status", "devices", "thanos", "config", "lead_screw1"];
    set(&backend, &lead_screw, json!("2"));
    assert_eq!(open_controller(&mut app).unwrap_err(), "KeyError: '2'", "text is not a map key");
    set(&backend, &lead_screw, json!([2]));
    assert!(open_controller(&mut app).unwrap_err().starts_with("KeyError:"));
    // Numbers equal to a key are that key: True is 1 (4mm/rev), 2.0 is 2 (5mm/rev).
    for (value, key) in [(json!(true), 1), (json!(2.0), 2)] {
        set(&backend, &lead_screw, value);
        open_controller(&mut app).unwrap();
        assert!(thanos(&app).thanos4u.unwrap().choices.contains(&(1, "lead_screw", key)));
    }
    remove(&backend, &["status", "devices", "thanos", "config"], "park_position");
    assert_eq!(open_controller(&mut app).unwrap_err(), "KeyError: 'park_position'");
    drop(app.take_threads());
}

#[test]
fn thanos4u_other_defaults_reach_actuators_one_to_seven() {
    let (_, mut app) = thanos4u();
    dispatch(&mut app, "dialog-button", "defaults:S", "").unwrap();
    let d = thanos(&app);
    let strokes = d.thanos4u.as_ref().unwrap().strokes.clone();
    assert!(strokes[..7].iter().all(|(_, mm)| *mm == 100), "{strokes:?}");
    assert_eq!(strokes[7], (8, 150), "actuator 8 keeps its stroke");
    for port in 1..5 {
        assert!(d.pending.contains(&(format!("stroke{port}"), 10)));
    }
    assert!(d.pending2.contains(&("stroke3".to_owned(), 10)));
    assert!(!d.pending2.iter().any(|(key, _)| key == "stroke4"));
    for (key, value) in [("park_position", 1), ("start_move_step", 5), ("6dof_start_pos", 127), ("estop_mode", 2)] {
        assert!(d.pending.contains(&(key.to_owned(), value)), "{key}");
    }
    assert!(d.pending.contains(&("ratio2".to_owned(), 1)) && d.pending.contains(&("lead_screw2".to_owned(), 2)));
    drop(app.take_threads());
}

#[test]
fn thanos4u_copy_reaches_actuators_two_to_seven() {
    let (_, mut app) = thanos4u();
    dispatch(&mut app, "dialog-field", "stroke:1", "200").unwrap();
    // 1:2, Expand, 10mm/rev.
    for (field, index) in [("var:1:ratio", "2"), ("var:1:inline", "1"), ("var:1:lead_screw", "2")] {
        dispatch(&mut app, "dialog-field", field, index).unwrap();
    }
    dispatch(&mut app, "dialog-button", "copy", "").unwrap();
    let d = thanos(&app);
    let s = d.thanos4u.as_ref().unwrap();
    for port in 2..8 {
        assert!(s.strokes.contains(&(port, 200)), "{port}");
        for choice in [(port, "ratio", 3), (port, "inline", 2), (port, "lead_screw", 3)] {
            assert!(s.choices.contains(&choice), "{choice:?}");
        }
    }
    assert!(s.strokes.contains(&(8, 150)), "actuator 8 is not copied");
    assert!(d.pending.contains(&("lead_screw2".to_owned(), 3)) && d.pending.contains(&("stroke3".to_owned(), 20)));
    drop(app.take_threads());
}

// ---- The PTA Motion System destinations ---------------------------------------------------------------------

fn pta(targets: Option<Value>) -> Option<(Arc<FakeBackend>, MyApp)> {
    crate::catalog::model("PTA Motion System")?;
    let backend = FakeBackend::new();
    match targets {
        Some(targets) => set(&backend, &["settings", "pt_actuator", "udp_targets"], targets),
        None => {
            set(&backend, &["settings", "pt_actuator", "udp_targets"], json!([]));
            remove(&backend, &["settings", "pt_actuator"], "udp_targets");
        }
    }
    let mut app = with_model(&backend, "PTA Motion System");
    open_controller(&mut app).unwrap();
    assert!(matches!(device(&app), Some(Device::Pta { .. })));
    Some((backend, app))
}

fn pta_rows(html: &str) -> Vec<(String, String)> {
    html.split("<tr><td class=\"mono\">")
        .skip(1)
        .map(|row| {
            let mut cells = row.split("</td>");
            let ip = cells.next().unwrap().to_owned();
            let port = cells.next().unwrap().trim_start_matches("<td class=\"mono\">").to_owned();
            (ip, port)
        })
        .collect()
}

#[test]
fn pta_dialog_lists_the_stored_destinations() {
    let Some((_, app)) = pta(Some(json!(["10.0.0.2:7409"]))) else { return };
    assert_eq!(pta_rows(&dialog_html(&app)), [("10.0.0.2".to_owned(), "7409".to_owned())]);
    let mut app = app;
    drop(app.take_threads());
}

#[test]
fn pta_dialog_with_a_none_setting_has_no_destinations_and_refuses_add() {
    let Some((_, mut app)) = pta(Some(Value::Null)) else { return };
    assert!(pta_rows(&dialog_html(&app)).is_empty());
    assert_eq!(
        dispatch(&mut app, "dialog-button", "add", "").unwrap_err(),
        "AttributeError: 'NoneType' object has no attribute 'append'"
    );
    drop(app.take_threads());
}

#[test]
fn pta_dialog_shows_malformed_destinations_as_they_are() {
    for (targets, rows) in [
        (json!(["broken"]), vec![("broken", "")]),
        (json!([null]), vec![("None", "")]),
        (json!(["10.0.0.1:7408", "broken"]), vec![("10.0.0.1", "7408"), ("broken", "")]),
    ] {
        let Some((_, mut app)) = pta(Some(targets)) else { return };
        let expected: Vec<(String, String)> = rows.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
        assert_eq!(pta_rows(&dialog_html(&app)), expected);
        drop(app.take_threads());
    }
}

/// The destination port box keeps a typed port within 1…49152; text that is no number keeps the port shown; a
/// new destination takes the port as the box shows it.
#[test]
fn the_pta_port_box_keeps_a_typed_port_within_its_limits() {
    let backend = FakeBackend::new();
    set(&backend, &["settings", "pt_actuator", "udp_targets"], json!([]));
    let mut app = hardware(&backend);
    open_pta(&mut app).unwrap();
    assert!(dialog_html(&app).contains("data-arg=\"port\" min=\"1\" max=\"49152\" step=\"1\""));
    let port = |app: &MyApp| match device(app) {
        Some(Device::Pta { port, .. }) => port.clone(),
        other => panic!("{other:?}"),
    };
    for (typed, shown) in
        [("70000", "49152"), ("0", "1"), ("-1", "1"), ("", "1"), ("8000.5", "8000.5"), ("abc", "8000.5")]
    {
        dispatch(&mut app, "dialog-field", "port", typed).unwrap();
        assert_eq!(port(&app), shown, "{typed:?}");
    }
    dispatch(&mut app, "dialog-field", "port", "60000").unwrap();
    assert!(dialog_html(&app).contains("type=\"number\" value=\"49152\""));
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert_eq!(doc(&backend, &["settings", "pt_actuator", "udp_targets"]), json!(["255.255.255.255:49152"]));
    drop(app.take_threads());
}

#[test]
fn pta_add_accepts_a_scoped_ipv6_address() {
    let Some((backend, mut app)) = pta(Some(json!([]))) else { return };
    dispatch(&mut app, "dialog-field", "ip", "fe80::1%eth0").unwrap();
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert_eq!(doc(&backend, &["settings", "pt_actuator", "udp_targets"]), json!(["fe80::1%eth0:7408"]));
    drop(app.take_threads());
}

#[test]
fn pta_destinations_without_the_setting_are_stored_on_ok_only() {
    let Some((backend, mut app)) = pta(None) else { return };
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert!(doc(&backend, &["settings", "pt_actuator"]).get("udp_targets").is_none(), "kept in the dialog");
    assert_eq!(pta_rows(&dialog_html(&app)).len(), 1);
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    assert_eq!(doc(&backend, &["settings", "pt_actuator", "udp_targets"]), json!(["255.255.255.255:7408"]));
    assert_eq!(called(&backend, Method::Post, "motion/disconnect").len(), 1);
    assert!(notice_texts(&app).iter().any(|(_, m)| m == "Motion platform reconnecting"));
    drop(app.take_threads());
}

#[test]
fn pta_delete_removes_and_stores_the_destination() {
    let Some((backend, mut app)) = pta(Some(json!(["10.0.0.1:7408"]))) else { return };
    dispatch(&mut app, "dialog-button", "delete:0", "").unwrap();
    assert_eq!(doc(&backend, &["settings", "pt_actuator", "udp_targets"]), json!([]));
    assert!(pta_rows(&dialog_html(&app)).is_empty());
    // A row that is gone changes nothing.
    dispatch(&mut app, "dialog-button", "delete:0", "").unwrap();
    assert_eq!(doc(&backend, &["settings", "pt_actuator", "udp_targets"]), json!([]));
    drop(app.take_threads());
}

#[test]
fn pta_ok_stores_before_the_reconnect() {
    let Some((backend, mut app)) = pta(Some(json!([]))) else { return };
    set(&backend, &["settings", "pt_actuator"], Value::Null);
    let error = dispatch(&mut app, "dialog-ok", "", "").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert!(called(&backend, Method::Post, "motion/disconnect").is_empty());
    drop(app.take_threads());
}

#[test]
fn pta_add_refuses_invalid_and_duplicate_addresses() {
    let Some((backend, mut app)) = pta(Some(json!(["255.255.255.255:7408"]))) else { return };
    backend.calls.lock().unwrap().clear();
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert!(alert_text(&app).unwrap().contains("destination already"));
    dispatch(&mut app, "dialog-field", "ip", "invalid").unwrap();
    dispatch(&mut app, "dialog-button", "add", "").unwrap();
    assert_eq!(alert_text(&app).unwrap(), "invalid is not a correctly formed IP address");
    assert_eq!(pta_rows(&dialog_html(&app)).len(), 1);
    assert!(backend.patches("settings").is_empty());
    drop(app.take_threads());
}

#[test]
fn pta_add_to_a_text_setting_is_refused() {
    let Some((_, mut app)) = pta(Some(json!(""))) else { return };
    assert!(pta_rows(&dialog_html(&app)).is_empty());
    assert_eq!(
        dispatch(&mut app, "dialog-button", "add", "").unwrap_err(),
        "AttributeError: 'str' object has no attribute 'append'"
    );
    drop(app.take_threads());
}

// ---- Axis Assignment ----------------------------------------------------------------------------------------

fn axis_app(model: &crate::catalog::ModelFlags) -> (Arc<FakeBackend>, MyApp) {
    let backend = FakeBackend::new();
    set(
        &backend,
        &["status", "motion", "assignment"],
        json!({"actuators": [{"key": 0, "axis": "1", "com_port": "COM3", "sn": "77"}], "status": false, "tl_port": ""}),
    );
    let app = with_model(&backend, &model.name);
    (backend, app)
}

fn open_axis(app: &mut MyApp) -> Result<(), String> {
    dispatch(app, "action", "Hardware:motion_model:0", "")
}

fn axis_rows(app: &MyApp) -> Vec<(String, usize)> {
    match device(app) {
        Some(Device::AxisAssignment { rows, .. }) => rows.clone(),
        other => panic!("the Axis Assignment, not {other:?}"),
    }
}

#[test]
fn axis_assignment_repeats_the_previous_actuator_for_options_left_at_none() {
    let model = crate::catalog::models().iter().find(|m| m.axis_assignment && m.assignment_options.len() >= 2).unwrap();
    let (backend, mut app) = axis_app(model);
    open_axis(&mut app).unwrap();
    assert!(dialog_html(&app).contains(">Actuator 0 - Axis 1 - COM3 - S/N 77<"));
    dispatch(&mut app, "dialog-field", "axis:0", "1").unwrap();
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let sent = called(&backend, Method::Post, "motion/axis-assignment");
    let assignment = sent[0].as_ref().unwrap()["assignment"].as_object().unwrap().clone();
    let count = model.assignment_options.len().min(model.motors as usize);
    assert_eq!(assignment.len(), count);
    assert!(assignment.values().all(|value| value == "1,77"), "{assignment:?}");
    drop(app.take_threads());
}

#[test]
fn axis_assignment_presets_none_without_a_stored_assignment() {
    let model = crate::catalog::models().iter().find(|m| m.axis_assignment).unwrap();
    let (backend, mut app) = axis_app(model);
    set(&backend, &["settings", "motion", "motor_axes"], Value::Null);
    open_axis(&mut app).unwrap();
    assert!(axis_rows(&app).iter().all(|(_, selected)| *selected == 0));
    drop(app.take_threads());
}

#[test]
fn axis_assignment_has_a_row_per_option_of_the_current_model() {
    let model = crate::catalog::models().iter().find(|m| m.assignment_options.len() == 2 && m.motors >= 2).unwrap();
    let (_, mut app) = axis_app(model);
    open_axis(&mut app).unwrap();
    let labels: Vec<String> = axis_rows(&app).into_iter().map(|(label, _)| label).collect();
    assert_eq!(labels, model.assignment_options);
    drop(app.take_threads());
}

#[test]
fn axis_assignment_presets_the_stored_actuator() {
    let model = crate::catalog::models().iter().find(|m| m.axis_assignment).unwrap();
    let (backend, mut app) = axis_app(model);
    set(
        &backend,
        &["status", "motion", "assignment", "actuators"],
        json!([{"key": 0, "axis": "1", "com_port": "COM4", "sn": "88"}]),
    );
    set(&backend, &["settings", "motion", "motor_axes"], json!({"0": "1,88"}));
    open_axis(&mut app).unwrap();
    assert_eq!(axis_rows(&app)[0].1, 1);
    assert!(dialog_html(&app).contains(">Actuator 0 - Axis 1 - COM4 - S/N 88<"));
    drop(app.take_threads());
}

#[test]
fn axis_assignment_refuses_a_malformed_stored_assignment() {
    let model = crate::catalog::models().iter().find(|m| m.axis_assignment).unwrap();
    let (backend, mut app) = axis_app(model);
    for (stored, error) in [
        (Value::Null, "AttributeError: 'NoneType' object has no attribute 'split'"),
        (json!("bad"), "IndexError: list index out of range"),
    ] {
        set(&backend, &["settings", "motion", "motor_axes"], json!({"0": stored}));
        assert_eq!(open_axis(&mut app).unwrap_err(), error);
        assert!(app.ui.dialog.is_none());
    }
    drop(app.take_threads());
}

#[test]
fn axis_assignment_needs_text_actuator_fields() {
    let model = crate::catalog::models().iter().find(|m| m.axis_assignment).unwrap();
    let (backend, mut app) = axis_app(model);
    set(
        &backend,
        &["status", "motion", "assignment", "actuators"],
        json!([{"key": 0, "axis": null, "com_port": null, "sn": "88"}]),
    );
    assert_eq!(open_axis(&mut app).unwrap_err(), "TypeError: can only concatenate str (not \"NoneType\") to str");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn axis_assignment_reports_a_refused_assignment() {
    let model = crate::catalog::models().iter().find(|m| m.axis_assignment).unwrap();
    let (backend, mut app) = axis_app(model);
    open_axis(&mut app).unwrap();
    dispatch(&mut app, "dialog-field", "axis:0", "1").unwrap();
    Probe::install(
        &mut app,
        &backend,
        Some((Method::Post, "motion/axis-assignment", "RuntimeError: assignment failed")),
    );
    assert_eq!(dispatch(&mut app, "dialog-ok", "", "").unwrap_err(), "RuntimeError: assignment failed");
    assert_eq!(called(&backend, Method::Post, "motion/axis-assignment").len(), 1);
    drop(app.take_threads());
}

// ---- The speaker matrix -------------------------------------------------------------------------------------

fn matrix_app(loaded_shakers: Value) -> (Arc<FakeBackend>, MyApp) {
    let backend = FakeBackend::new();
    let app = hardware(&backend);
    set(&backend, &["loaded_shakers"], loaded_shakers);
    (backend, app)
}

fn open_matrix_of_sub(app: &mut MyApp) -> Result<(), String> {
    dispatch(app, "action", "Hardware:sub:1", "")
}

fn visible_effects() -> Vec<String> {
    openrigcue_config::catalog::registry::Registry::builtin()
        .shaker_channel_effects
        .iter()
        .filter(|effect| effect.visible && crate::CHANNEL_PARENTS.contains(&effect.parent.as_str()))
        .map(|effect| effect.id.clone())
        .collect()
}

fn matrix_value(html: &str, effect: &str) -> String {
    let start = html.find(&format!("id=\"matrix-{effect}\" type=\"range\"")).unwrap_or_else(|| panic!("{effect}"));
    let rest = &html[start..];
    let value = &rest[rest.find(" value=\"").unwrap() + 8..];
    value[..value.find('"').unwrap()].to_owned()
}

#[test]
fn speaker_matrix_needs_the_speaker_in_the_loaded_layout() {
    let (_, mut app) = matrix_app(json!({}));
    assert_eq!(open_matrix_of_sub(&mut app).unwrap_err(), "KeyError: 'SUB'");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

#[test]
fn speaker_matrix_shows_the_loaded_levels_and_zero_for_missing_effects() {
    let (_, mut app) = matrix_app(json!({"SUB": {"sfx": {"engine": 4}}}));
    open_matrix_of_sub(&mut app).unwrap();
    let html = dialog_html(&app);
    assert_eq!(matrix_value(&html, "engine"), "4");
    for effect in visible_effects().iter().filter(|effect| *effect != "engine") {
        assert_eq!(matrix_value(&html, effect), "0", "{effect}");
    }
    drop(app.take_threads());
}

#[test]
fn speaker_matrix_levels_must_be_numbers() {
    let (backend, mut app) = matrix_app(json!({"SUB": {"sfx": {"engine": 4}}}));
    open_matrix_of_sub(&mut app).unwrap();
    let error = dispatch(&mut app, "dialog-field", "level:engine", "NaN").unwrap_err();
    assert!(error.starts_with("ValueError:"), "{error}");
    assert_eq!(matrix_value(&dialog_html(&app), "engine"), "4");
    assert!(called(&backend, Method::Put, "shakers/speakers/SUB/matrix").is_empty());
    drop(app.take_threads());
}

#[test]
fn speaker_matrix_sends_levels_as_whole_numbers_and_keeps_the_others() {
    let levels: serde_json::Map<String, Value> = openrigcue_config::catalog::registry::Registry::builtin()
        .shaker_channel_effects
        .iter()
        .map(|effect| (effect.id.clone(), json!(true)))
        .collect();
    let (backend, mut app) = matrix_app(json!({"SUB": {"sfx": levels}}));
    open_matrix_of_sub(&mut app).unwrap();
    assert_eq!(matrix_value(&dialog_html(&app), "engine"), "1", "a stored true is level 1");
    dispatch(&mut app, "dialog-field", "level:engine", "7").unwrap();
    assert_eq!(matrix_value(&dialog_html(&app), "engine"), "7");
    dispatch(&mut app, "dialog-ok", "", "").unwrap();
    let sent = called(&backend, Method::Put, "shakers/speakers/SUB/matrix");
    let sent = sent[0].as_ref().unwrap();
    assert_eq!(sent["engine"], json!(7));
    assert!(sent
        .as_object()
        .unwrap()
        .iter()
        .filter(|(key, _)| *key != "engine")
        .all(|(_, value)| *value == json!(true)));
    drop(app.take_threads());
}

#[test]
fn speaker_matrix_needs_a_level_table() {
    let (_, mut app) = matrix_app(json!({"SUB": {"sfx": null}}));
    assert_eq!(open_matrix_of_sub(&mut app).unwrap_err(), "AttributeError: 'NoneType' object has no attribute 'copy'");
    assert!(app.ui.dialog.is_none());
    drop(app.take_threads());
}

// ---- The output tests ---------------------------------------------------------------------------------------

fn start(app: &mut MyApp, action: &str) -> Result<(), String> {
    dispatch(app, "action", action, "")?;
    dispatch(app, "dialog-button", "start", "")
}

fn thread_names(app: &mut MyApp) -> Vec<&'static str> {
    app.take_threads().iter().map(|thread| thread.name).collect()
}

/// The iFlag, HapticR (enabled) and SIM3D (a four-port model) connected, so that their tests can run.
fn connect_test_devices(backend: &Arc<FakeBackend>) {
    set(backend, &["settings", "app", "enable_hapticr"], json!(true));
    set(backend, &["settings", "sim3d", "model"], json!("4 Ports"));
    for device in ["iflag", "hapticr", "sim3d"] {
        set(backend, &["status", "accessories", device], json!("Connected"));
    }
}

#[test]
fn stale_test_dialogs_start_no_test_once_a_game_runs_or_the_device_goes() {
    for action in ["Hardware:iflag_test:", "Hardware:haptic_test_front:", "Hardware:rumble_port2:0"] {
        for (change, path, value) in [
            ("a game starts", &["status", "session", "game"][..], json!("acc")),
            ("the device goes", &["status", "accessories", "iflag"][..], json!("Not Connected")),
            ("the device goes", &["status", "accessories", "hapticr"][..], json!("Not Connected")),
            ("the device goes", &["status", "accessories", "sim3d"][..], json!("Not Connected")),
            ("HapticR is turned off", &["settings", "app", "enable_hapticr"][..], json!(false)),
            ("the SIM3D model is cleared", &["settings", "sim3d", "model"][..], json!("None")),
        ] {
            let device = match action {
                "Hardware:iflag_test:" => "iflag",
                "Hardware:haptic_test_front:" => "hapticr",
                _ => "sim3d",
            };
            let applies = change == "a game starts"
                || path.contains(&device)
                || (device == "hapticr" && path.contains(&"enable_hapticr"))
                || (device == "sim3d" && path.contains(&"model"));
            let backend = FakeBackend::new();
            connect_test_devices(&backend);
            let mut app = hardware(&backend);
            dispatch(&mut app, "action", action, "").unwrap();
            assert!(dialog_html(&app).contains("data-arg=\"start\""), "{action}");
            set(&backend, path, value);
            app.refresh_docs();
            dispatch(&mut app, "dialog-button", "start", "").unwrap();
            assert_eq!(app.ui.devices.hardware.test.is_none(), applies, "{action}: {change} {path:?}");
            if applies {
                assert!(app.take_threads().is_empty(), "{action}: {change}");
                let html = dialog_html(&app);
                let start = html.split("data-arg=\"start\"").nth(1).unwrap().split('>').next().unwrap();
                assert!(start.contains("disabled"), "{action}: {change}: Start test is offered no more");
            }
            drop(app.take_threads());
        }
    }
}

/// An open iFlag or HapticR test dialog starts no test while a test of its device runs, started elsewhere: Start
/// is disabled and refused, as the row buttons are; the other device's test does not block it.
#[test]
fn test_dialogs_start_no_test_while_a_test_of_their_device_runs_elsewhere() {
    for action in ["Hardware:iflag_test:", "Hardware:haptic_test_front:"] {
        for (key, value) in [("iflag_test", json!(true)), ("haptic_test", json!(2))] {
            let applies = action.contains(key);
            let backend = FakeBackend::new();
            connect_test_devices(&backend);
            let mut app = hardware(&backend);
            dispatch(&mut app, "action", action, "").unwrap();
            set(&backend, &["status", "controls", "tests", key], value.clone());
            app.refresh_docs();
            let html = dialog_html(&app);
            let start = html.split("data-arg=\"start\"").nth(1).unwrap().split('>').next().unwrap();
            assert_eq!(start.contains("disabled"), applies, "{action}: {key} running");
            dispatch(&mut app, "dialog-button", "start", "").unwrap();
            assert_eq!(app.ui.devices.hardware.test.is_none(), applies, "{action}: {key} running");
            if applies {
                assert!(app.take_threads().is_empty(), "{action}: {key}");
                assert_eq!(doc(&backend, &["status", "controls", "tests", key]), value, "{action}: left as it runs");
            }
            drop(app.take_threads());
        }
    }
}

#[test]
fn device_tests_start_their_outputs_and_one_timer() {
    let backend = FakeBackend::new();
    connect_test_devices(&backend);
    let mut app = hardware(&backend);
    start(&mut app, "Hardware:wind_type:0").unwrap();
    assert_eq!(thread_names(&mut app), ["hardware_test"]);
    assert_eq!(app.ui.devices.hardware.test.as_ref().unwrap().target, Test::Wind);
    dispatch(&mut app, "dialog-button", "stop", "").unwrap();
    start(&mut app, "Hardware:iflag_test:").unwrap();
    assert_eq!(thread_names(&mut app), ["hardware_test"]);
    assert!(backend.patches("controls").contains(&json!({"tests": {"iflag_test": true}})));
    assert!(dialog_html(&app).contains("Testing…"));
}

#[test]
fn a_refused_test_output_starts_no_test() {
    for action in ["Hardware:iflag_test:", "Hardware:haptic_test_front:", "Hardware:rumble_port2:0"] {
        let backend = FakeBackend::new();
        connect_test_devices(&backend);
        let mut app = hardware(&backend);
        set(&backend, &["control_failure"], json!("TypeError: fixture parameter assignment failed"));
        assert_eq!(start(&mut app, action).unwrap_err(), "TypeError: fixture parameter assignment failed", "{action}");
        assert!(app.take_threads().is_empty(), "{action}");
        assert!(app.ui.devices.hardware.test.is_none(), "{action}");
    }
}

#[test]
fn hapticr_tests_use_the_wheel_numbers() {
    for (action, wheel) in [
        ("Hardware:haptic_test_front:", 1),
        ("Hardware:haptic_test_front:0", 2),
        ("Hardware:haptic_test_rear:", 3),
        ("Hardware:haptic_test_rear:0", 4),
    ] {
        let backend = FakeBackend::new();
        set(&backend, &["settings", "app", "enable_hapticr"], json!(true));
        set(&backend, &["status", "accessories", "hapticr"], json!("Connected"));
        let mut app = hardware(&backend);
        start(&mut app, action).unwrap();
        assert_eq!(doc(&backend, &["status", "controls", "tests", "haptic_test"]), json!(wheel), "{action}");
        assert_eq!(thread_names(&mut app), ["hardware_test"]);
        let html = group(&mut app, "HapticR");
        assert!(html.contains("data-arg=\"Hardware:haptic_test_front:0\" disabled"));
        assert!(html.contains("data-arg=\"Hardware:haptic_test_rear:0\" disabled"));
    }
}

#[test]
fn rumble_test_disables_every_port_test_while_it_runs() {
    let backend = FakeBackend::new();
    set(&backend, &["settings", "sim3d", "model"], json!("4 Ports"));
    set(&backend, &["status", "accessories", "sim3d"], json!("Connected"));
    let mut app = hardware(&backend);
    let html = group(&mut app, "SIM3D");
    assert!(!html.contains("data-arg=\"Hardware:rumble_port3:0\" disabled"));
    start(&mut app, "Hardware:rumble_port3:0").unwrap();
    assert_eq!(doc(&backend, &["status", "controls", "tests", "rumble_test"]), json!(3));
    let html = screen::main(&app);
    for port in 1..5 {
        assert!(html.contains(&format!("data-arg=\"Hardware:rumble_port{port}:0\" disabled")), "{port}");
    }
    assert_eq!(thread_names(&mut app), ["hardware_test"]);
}

#[test]
fn intellibox_tests_restore_the_outputs_they_found() {
    for (action, switch) in
        [("Hardware:wind_type:0", "wind"), ("Hardware:nbr_leds:0", "led"), ("Hardware:shift_light_dim_perc:0", "tach")]
    {
        let backend = FakeBackend::new();
        set(&backend, &["status", "controls", "switches", switch], json!("off"));
        let mut app = hardware(&backend);
        start(&mut app, action).unwrap();
        assert_eq!(doc(&backend, &["status", "controls", "switches", switch]), json!("always_on"), "{switch}");
        dispatch(&mut app, "dialog-button", "stop", "").unwrap();
        assert_eq!(doc(&backend, &["status", "controls", "switches", switch]), json!("off"), "{switch}");
        let html = screen::main(&app);
        for argument in ["wind_type:0", "nbr_leds:0", "shift_light_dim_perc:0"] {
            assert!(!html.contains(&format!("data-arg=\"Hardware:{argument}\" disabled")), "{argument} offered again");
        }
        drop(app.take_threads());
    }
}

#[test]
fn intellibox_test_needs_readable_outputs() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    set(&backend, &["status", "controls", "switches"], Value::Null);
    let error = start(&mut app, "Hardware:wind_type:0").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert!(backend.patches("controls").is_empty(), "no output is forced");
    assert!(app.ui.devices.hardware.test.is_none());
    // A later output that cannot be read leaves the earlier one forced.
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    remove(&backend, &["status", "controls", "switches"], "wind_center");
    assert_eq!(start(&mut app, "Hardware:wind_type:0").unwrap_err(), "KeyError: 'wind_center'");
    assert_eq!(doc(&backend, &["status", "controls", "switches", "wind"]), json!("always_on"));
    assert!(app.take_threads().is_empty());
}

#[test]
fn intellibox_test_stops_at_a_refused_output() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    set(&backend, &["control_failure"], json!("TypeError: fixture assignment failed"));
    assert_eq!(start(&mut app, "Hardware:wind_type:0").unwrap_err(), "TypeError: fixture assignment failed");
    assert_eq!(backend.patches("controls"), vec![json!({"switches": {"wind": "always_on"}})]);
    assert_eq!(app.docs.controls["switches"]["wind"], json!("auto"));
    assert!(app.ui.devices.hardware.test.is_none());
    assert!(app.take_threads().is_empty());
}

#[test]
fn a_failed_restore_ends_the_test_with_its_outputs_as_they_are() {
    for (action, path, running) in [
        ("Hardware:wind_type:0", ["switches", "wind"], json!("always_on")),
        ("Hardware:iflag_test:", ["tests", "iflag_test"], json!(true)),
        ("Hardware:haptic_test_front:", ["tests", "haptic_test"], json!(1)),
        ("Hardware:rumble_port1:0", ["tests", "rumble_test"], json!(1)),
    ] {
        let backend = FakeBackend::new();
        connect_test_devices(&backend);
        let mut app = hardware(&backend);
        start(&mut app, action).unwrap();
        drop(app.take_threads());
        set(&backend, &["control_failure"], json!("TypeError: fixture reset failed"));
        assert_eq!(dispatch(&mut app, "dialog-button", "stop", "").unwrap_err(), "TypeError: fixture reset failed");
        assert_eq!(doc(&backend, &["status", "controls", path[0], path[1]]), running, "{action}");
        assert!(app.ui.devices.hardware.test.is_none(), "{action}");
    }
}

#[test]
fn finished_iflag_and_rumble_tests_reset_and_enable_their_buttons() {
    let backend = FakeBackend::new();
    set(&backend, &["settings", "sim3d", "model"], json!("4 Ports"));
    set(&backend, &["status", "accessories", "sim3d"], json!("Connected"));
    set(&backend, &["status", "accessories", "iflag"], json!("Connected"));
    let mut app = hardware(&backend);
    start(&mut app, "Hardware:iflag_test:").unwrap();
    assert!(group(&mut app, "IFlag").contains("data-arg=\"Hardware:iflag_test:\" disabled"));
    screen::finish_test(&mut app).unwrap();
    assert_eq!(doc(&backend, &["status", "controls", "tests", "iflag_test"]), json!(false));
    assert!(!screen::main(&app).contains("data-arg=\"Hardware:iflag_test:\" disabled"));
    start(&mut app, "Hardware:rumble_port2:0").unwrap();
    screen::finish_test(&mut app).unwrap();
    assert_eq!(doc(&backend, &["status", "controls", "tests", "rumble_test"]), json!(0));
    let html = group(&mut app, "SIM3D");
    for port in 1..5 {
        assert!(!html.contains(&format!("data-arg=\"Hardware:rumble_port{port}:0\" disabled")), "{port}");
    }
    drop(app.take_threads());
}

#[test]
fn speaker_test_reads_the_stored_sound_device_and_reports_a_failure() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    app.docs.settings["shakers"]["sound_card"] = json!("None");
    set(&backend, &["settings", "shakers", "sound_card"], json!("current-card"));
    Probe::install(
        &mut app,
        &backend,
        Some((Method::Post, "shakers/speakers/SUB/test", "TypeError: speaker test failure")),
    );
    assert_eq!(start(&mut app, "Hardware:sub:0").unwrap_err(), "TypeError: speaker test failure");
    assert_eq!(called(&backend, Method::Post, "shakers/speakers/SUB/test").len(), 1);
    // Without a sound device nothing plays.
    set(&backend, &["settings", "shakers", "sound_card"], json!("None"));
    start(&mut app, "Hardware:sub:0").unwrap();
    assert_eq!(called(&backend, Method::Post, "shakers/speakers/SUB/test").len(), 1);
    assert!(app.take_threads().is_empty(), "a speaker test has no timer");
}

// ---- The sound device ---------------------------------------------------------------------------------------

fn choose_sound_device(app: &mut MyApp) -> Result<(), String> {
    let html = group(app, "Shakers");
    let index = options(&html, "c-hardware-soundcard").iter().position(|name| name == "USB Sound Device").unwrap();
    dispatch(app, "set", "Hardware:soundcard", &index.to_string())
}

#[test]
fn sound_device_choice_needs_the_settings_section() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    group(&mut app, "Shakers");
    set(&backend, &["settings", "shakers"], Value::Null);
    let error = dispatch(&mut app, "set", "Hardware:soundcard", "1").unwrap_err();
    assert!(error.starts_with("TypeError:"), "{error}");
    assert!(called(&backend, Method::Put, "shakers/soundcard").is_empty());
    drop(app.take_threads());
}

#[test]
fn sound_device_open_failure_keeps_the_choice_and_warns() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    set(&backend, &["loaded_shakers"], json!({"SUB": {"enable": true}}));
    Probe::install(
        &mut app,
        &backend,
        Some((Method::Put, "shakers/soundcard", "RuntimeError: fixture device unavailable")),
    );
    choose_sound_device(&mut app).unwrap();
    assert_eq!(doc(&backend, &["settings", "shakers", "sound_card"]), json!("USB Sound Device"));
    assert_eq!(
        alert_text(&app).unwrap(),
        "Could not open the selected sound device: RuntimeError: fixture device unavailable"
    );
    let html = screen::main(&app);
    assert!(!html.contains("data-arg=\"Hardware:sub:0\" disabled"), "the speaker rows follow the stored device");
    // The Sound Device row shows None while the stored device does not open, and the device once it opens.
    let selected = |html: &str| {
        let start = html.find("id=\"c-hardware-soundcard\"").unwrap();
        let select = &html[start..start + html[start..].find("</select>").unwrap()];
        select.split("<option").find(|option| option.contains(" selected")).map(|option| {
            let text = &option[option.find('>').unwrap() + 1..];
            text.split("</option>").next().unwrap().to_owned()
        })
    };
    assert_eq!(selected(&html).as_deref(), Some("None"));
    app.backend = backend.clone();
    choose_sound_device(&mut app).unwrap();
    assert_eq!(selected(&screen::main(&app)).as_deref(), Some("USB Sound Device"));
    drop(app.take_threads());
}

#[test]
fn sound_device_choice_is_opened_every_time() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    choose_sound_device(&mut app).unwrap();
    choose_sound_device(&mut app).unwrap();
    assert_eq!(
        called(&backend, Method::Put, "shakers/soundcard"),
        vec![Some(json!({"name": "USB Sound Device"})), Some(json!({"name": "USB Sound Device"}))]
    );
    drop(app.take_threads());
}

#[test]
fn a_sound_device_refresh_lists_reopens_and_clears_the_request() {
    for case in ["success", "not requested", "list failure", "bad second endpoint", "missing selection"] {
        let backend = FakeBackend::new();
        let mut app = hardware(&backend);
        {
            let mut docs = backend.docs.lock().unwrap();
            docs["status"]["shakers"]["refresh_soundcard"] = json!(case != "not requested");
            docs["hardware"]["sound_cards"] = json!(["None", "Fresh Device"]);
            match case {
                "list failure" => docs["hardware_failure"] = json!("list failed"),
                "bad second endpoint" => {
                    docs["hardware"]["sound_devices"] = json!([{"id": "none", "name": "None"}, {"id": "next"}])
                }
                "missing selection" => {
                    docs["settings"]["shakers"].as_object_mut().unwrap().shift_remove("sound_card");
                }
                _ => {}
            }
        }
        backend.calls.lock().unwrap().clear();
        screen::rules(&mut app).unwrap();
        let listed = called(&backend, Method::Get, "hardware").len();
        let opened = called(&backend, Method::Put, "shakers/soundcard").len();
        let cleared = called(&backend, Method::Delete, "shakers/refresh-soundcard").len();
        let html = group(&mut app, "Shakers");
        let shown = options(&html, "c-hardware-soundcard");
        match case {
            "not requested" => {
                assert_eq!((listed, opened, cleared), (0, 0, 0));
                assert!(shown.contains(&"USB Sound Device".to_owned()));
            }
            "success" => {
                assert_eq!((listed, opened, cleared), (1, 1, 1));
                assert!(shown.contains(&"Fresh Device".to_owned()) && !shown.contains(&"USB Sound Device".to_owned()));
            }
            "list failure" => {
                // The device is not reopened and the request stays for the next pass.
                assert_eq!((listed, opened, cleared), (1, 0, 0));
                assert!(shown.contains(&"USB Sound Device".to_owned()), "the previous list stays");
                assert_eq!(doc(&backend, &["status", "shakers", "refresh_soundcard"]), json!(true));
                drop(app.take_threads());
                continue;
            }
            "bad second endpoint" => {
                assert_eq!((opened, cleared), (1, 1));
                assert_eq!(shown.iter().filter(|name| *name != "—").collect::<Vec<_>>(), ["None"]);
            }
            _ => {
                assert_eq!((opened, cleared), (0, 1));
                assert!(alert_text(&app).unwrap().starts_with("Could not open the selected sound device: "));
            }
        }
        assert_eq!(doc(&backend, &["status", "shakers", "refresh_soundcard"]), json!(false), "{case}");
        drop(app.take_threads());
    }
}

// ---- The main loop's test rules -----------------------------------------------------------------------------

#[test]
fn a_refused_motion_test_reset_stops_the_rules() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    set(&backend, &["status", "controls", "tests", "motion_test"], json!(true));
    set(&backend, &["status", "controls", "tests", "belt_test"], json!(true));
    set(&backend, &["status", "controls", "switches", "belt"], json!(false));
    set(&backend, &["control_failure"], json!("TypeError: fixture mode reset failed"));
    assert_eq!(screen::rules(&mut app).unwrap_err(), "TypeError: fixture mode reset failed");
    assert_eq!(doc(&backend, &["status", "controls", "tests", "motion_test"]), json!(true));
    assert_eq!(doc(&backend, &["status", "controls", "tests", "belt_test"]), json!(true), "the later rules wait");
    drop(app.take_threads());
}

#[test]
fn a_running_game_and_the_belt_switched_off_end_the_belt_test() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    {
        let mut docs = backend.docs.lock().unwrap();
        docs["status"]["session"]["game"] = json!("racing");
        docs["status"]["controls"]["switches"]["belt"] = json!(false);
        docs["status"]["controls"]["tests"]["belt_test"] = json!(true);
        docs["status"]["controls"]["tests"]["belt_homing"] = json!(3);
        docs["status"]["controls"]["tests"]["belt_test_value"] = json!(17);
    }
    screen::rules(&mut app).unwrap();
    let tests = doc(&backend, &["status", "controls", "tests"]);
    assert_eq!((tests["belt_test"].clone(), tests["belt_homing"].clone()), (json!(false), json!(0)));
    assert_eq!(tests["belt_test_value"].as_f64(), Some(0.0));
    drop(app.take_threads());
}

#[test]
fn the_rules_pass_reads_the_current_guards() {
    let backend = FakeBackend::new();
    let mut app = hardware(&backend);
    let probe = Probe::install(&mut app, &backend, None);
    for _ in 0..2 {
        probe.reads.lock().unwrap().clear();
        screen::rules(&mut app).unwrap();
        let mut expected = vec!["MOTIONBOX.model".to_owned()];
        expected.extend(
            ["pitch", "roll", "yaw", "sway", "surge", "heave", "traction_loss", "traction_loss_front"]
                .map(|key| format!("tests.motion_sliders.{key}")),
        );
        expected.extend(["game".to_owned(), "switches.belt".to_owned()]);
        assert_eq!(*probe.reads.lock().unwrap(), expected);
    }
    // With a model, the link status and the game guard the motion test.
    let model = crate::catalog::models().iter().find(|m| m.name != "None").unwrap();
    app.backend = backend.clone();
    screen::choose_model(&mut app, &model.name).unwrap();
    set(&backend, &["status", "motion", "link", "status"], json!("Connected"));
    let probe = Probe::install(&mut app, &backend, None);
    screen::rules(&mut app).unwrap();
    assert_eq!(probe.reads.lock().unwrap()[..3], ["MOTIONBOX.model", "motion_status", "game"]);
    drop(app.take_threads());
}

#[test]
fn the_motion_test_row_follows_the_current_link_status() {
    let backend = FakeBackend::new();
    let model = crate::catalog::models().iter().find(|m| m.name != "None").unwrap();
    let mut app = with_model(&backend, &model.name);
    assert!(group(&mut app, "Motion").contains("data-arg=\"Hardware:motion_test:\" disabled"));
    set(&backend, &["status", "motion", "link", "status"], json!("Connected"));
    assert!(!screen::main(&app).contains("data-arg=\"Hardware:motion_test:\" disabled"));
    drop(app.take_threads());
}

// ---- HapticR group, manual motion ports -----------------------------------------------------------------------

#[test]
fn hapticr_group_follows_a_yes_text_setting() {
    let backend = FakeBackend::new();
    set(&backend, &["settings", "app", "enable_hapticr"], json!("y"));
    let app = hardware(&backend);
    assert!(screen::main(&app).contains(">HapticR</option>"));
    let mut app = app;
    drop(app.take_threads());
}

#[test]
fn an_unreadable_hapticr_setting_hides_the_group() {
    let backend = FakeBackend::new();
    set(&backend, &["settings", "app", "enable_hapticr"], json!(true));
    let mut app = hardware(&backend);
    assert!(screen::main(&app).contains(">HapticR</option>"));
    Probe::failing_read(&mut app, &backend, "APP.enable_hapticr");
    let html = screen::main(&app);
    assert!(!html.contains(">HapticR</option>") && html.contains(">Intellibox</option>"));
    drop(app.take_threads());
}

#[test]
fn manual_motion_ports_skip_ports_without_a_usable_hardware_id() {
    let backend = FakeBackend::new();
    set(
        &backend,
        &["hardware", "serial_ports"],
        json!([
            {"device": "COM8", "description": "FT232R", "hwid": "USB VID:PID=0403:6001"},
            {"device": "COM9", "description": "PNP", "hwid": null},
            {"device": "COM10", "description": "later FT232R", "hwid": "USB VID:PID=0403:6001"},
            {"device": "COM7", "description": "empty", "hwid": ""},
            {"device": "COM6", "description": "literal", "hwid": "None"},
            {"device": "COM5", "description": "other", "hwid": "USB VID:PID=0403:6015"}
        ]),
    );
    let mut app = with_model(&backend, "T1000-5M PRS200 (Upgrade)");
    assert_eq!(doc(&backend, &["settings", "motion", "model"]), json!("T1000-5M PRS200 (Upgrade)"));
    let html = group(&mut app, "Motion");
    for id in ["c-hardware-motion_port", "c-hardware-motion_port2"] {
        let shown: Vec<String> = options(&html, id).into_iter().filter(|name| name != "—").collect();
        assert_eq!(shown, ["COM8", "COM10"], "{id}");
    }
    drop(app.take_threads());
}

#[test]
fn manual_motion_ports_leave_out_held_ports_and_list_any_port_for_direct_controllers() {
    for (manufacturer, claim) in [
        ("PROSIMU", ""),
        ("VNM SIMULATION", ""),
        ("PT-ACTUATOR", ""),
        ("VNM SIMULATION", "accessory"),
        ("VNM SIMULATION", "traction"),
    ] {
        let backend = FakeBackend::new();
        set(&backend, &["hardware", "serial_ports"], json!([{"device": "COM9", "description": "PNP", "hwid": null}]));
        match claim {
            "accessory" => set(&backend, &["status", "accessories", "iflag_port"], json!("COM9")),
            "traction" => set(&backend, &["status", "motion", "assignment"], json!({"tl_port": "COM9"})),
            _ => {}
        }
        let model =
            crate::catalog::models().iter().find(|m| m.manufacturer == manufacturer && m.manual_com_port).unwrap();
        let mut app = with_model(&backend, &model.name);
        let html = group(&mut app, "Motion");
        let shown: Vec<String> = options(&html, "c-hardware-motion_port").into_iter().filter(|n| n != "—").collect();
        let listed = manufacturer != "PROSIMU" && claim.is_empty();
        assert_eq!(shown.contains(&"COM9".to_owned()), listed, "{manufacturer} {claim}");
        drop(app.take_threads());
    }
}

// ---- Every text of the device dialogs ---------------------------------------------------------------------------

#[test]
fn every_device_dialog_text_has_an_entry_in_every_language() {
    let mut looked_up = std::collections::BTreeSet::new();
    let mut collect = |backend: &FakeBackend, app: &MyApp| {
        crate::ui::render(app, Region::Dialog);
        looked_up.extend(backend.translations.lock().unwrap().iter().cloned());
    };
    // VNM with its Reverse switches.
    let backend = FakeBackend::new();
    let vnm = crate::catalog::models()
        .iter()
        .filter(|m| m.manufacturer == "VNM SIMULATION")
        .max_by_key(|m| m.port_mapping.len())
        .unwrap();
    let mut app = with_model(&backend, &vnm.name);
    open_vnm(&mut app).unwrap();
    collect(&backend, &app);
    // DOF Reality, the layout removal and the MagicBox confirmations.
    open_dof(&mut app).unwrap();
    collect(&backend, &app);
    for dialog in [
        Device::Remove { name: "My Layout".into() },
        Device::Controller { wheel_id: String::new(), confirm: Some(true) },
        Device::Controller { wheel_id: String::new(), confirm: Some(false) },
    ] {
        open_device(&mut app, dialog);
        collect(&backend, &app);
    }
    // The Yaw VR limits.
    set(
        &backend,
        &["status", "devices", "yaw_vr"],
        json!({"centered": false, "device_model": "YAWDEVICE", "limits": {"power": 17, "pitch_fwd_limit": 10,
               "pitch_bwd_limit": 11, "roll_limit": 12, "yaw_limit": 90, "yaw_limit_enabled": 1, "vibration_enabled": 0}}),
    );
    open_yaw_vr(&mut app).unwrap();
    collect(&backend, &app);
    drop(app.take_threads());
    // The Thanos4U controller with its two confirmations, then the PT-ACTUATOR table.
    let (backend, mut app) = thanos4u();
    collect(&backend, &app);
    for spike in [false, true] {
        let mut d = thanos(&app);
        d.confirm = Some(spike);
        open_device(&mut app, Device::Thanos(Box::new(d)));
        collect(&backend, &app);
    }
    screen::choose_model(&mut app, &pt_model().name).unwrap();
    open_controller(&mut app).unwrap();
    collect(&backend, &app);
    drop(app.take_threads());
    // PTA Motion System.
    if let Some((backend, mut app)) = pta(Some(json!(["10.0.0.1:7408"]))) {
        collect(&backend, &app);
        drop(app.take_threads());
    }
    // The Axis Assignment.
    let model = crate::catalog::models().iter().find(|m| m.axis_assignment).unwrap();
    let (backend, mut app) = axis_app(model);
    open_axis(&mut app).unwrap();
    collect(&backend, &app);
    drop(app.take_threads());

    for text in [
        "Actuator ^number^ - Axis ^axis^ - ^port^ - S/N ^serial^",
        "Please confirm that this profile should be deleted: ^name^",
        "Limit Yaw",
        "Can ID",
        "Reboot?",
        "Shutdown?",
        "Kill Motor Switch",
        "Max Angle Confirmation",
        "Spike Filter Confirmation",
        "ACTUATOR ^name^",
        "Auto Set KI and KP",
        "Reverse",
    ] {
        assert!(looked_up.contains(text), "{text} was looked up");
    }
    let missing =
        crate::app::tests::texts_without_entry(&looked_up, &["My Layout", "10.0.0.1:7408", "USB Sound Device"]);
    assert_eq!(missing, Vec::<String>::new());
}
